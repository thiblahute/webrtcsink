// Implements https://datatracker.ietf.org/doc/html/draft-ietf-rmcat-gcc-02
use async_std::task;
use atomic_refcell::AtomicRefCell;
use chrono::Duration;
use debug_ignore::DebugIgnore as DI;
use futures::{prelude::*, channel::mpsc::{Sender}};
use gst::{glib::{self},
    prelude::*, subclass::prelude::*,
};
use gst::glib::clone::Downgrade;
use once_cell::sync::Lazy;
use std::{
    collections::VecDeque,
    fmt,
    fmt::Debug,
    mem,
    sync::{Arc, Mutex},
    time,
};

use super::Bitrate;

const DEFAULT_MIN_BITRATE: Bitrate = 30_000;
const DEFAULT_START_BITRATE: Bitrate = 2_048_000;
const DEFAULT_MAX_BITRATE: Bitrate = 8_192_000;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsink-gcc",
        gst::DebugColorFlags::empty(),
        Some("Google Congestion Controller"),
    )
});

// Table1. Time limit in milliseconds  between packet bursts which  identifies a group
static BURST_TIME: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(5));

// Table1. Coefficient used for the measured noise variance
//  [0.1,0.001]
const CHI: f64 = 0.001;
const ONE_MINUS_CHI: f64 = 1. - CHI;

// Table1. State noise covariance matrix
const Q: f64 = 0.001;

// Table1. Initial value for the adaptive threshold
static INITIAL_DEL_VAR_TH: Lazy<Duration> = Lazy::new(|| Duration::microseconds(12500));

// Table1. Initial value of the system error covariance
const INITIAL_ERROR_COVARIANCE: f64 = 0.1;

// Table1. Time required to trigger an overuse signal
static OVERUSE_TIME_TH: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(10));

// from 5.5 "beta is typically chosen to be in the interval [0.8, 0.95], 0.85 is the RECOMMENDED value."
const BETA: f64 = 0.85;

// This is the `K` from section "5.3 Arrival-time filter" in expression:
// "where `f_max = max {1/(inter_arrival_time[T(j) - T(j-1))]}` for j in i-K+1,...,i"
const LAST_INTER_DEPARTURE_SIZE: usize = 25;

// From "5.5 Rate control"
// It is RECOMMENDED to measure this average and standard deviation with an
// exponential moving average with the smoothing factor 0.95, as it is
// expected that this average covers multiple occasions at which we are
// in the Decrease state.
const MOVING_AVERAGE_SMOOTHING_FACTOR: f64 = 0.5; // FIXME: Should be 0.95

// `N(i)` is the number of packets received the past T seconds and `L(j)` is
// the payload size of packet j.  A window between 0.5 and 1 second is
// RECOMMENDED.
static PACKETS_RECEIVED_WINDOW: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(1000)); // ms

// from "5.4 Over-use detector" ->
// Moreover, del_var_th(i) SHOULD NOT be updated if this condition
// holds:
//
// ```
// |m(i)| - del_var_th(i) > 15
// ```
static MAX_M_MINUS_DEL_VAR_TH: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(15));

// from 5.4 "It is also RECOMMENDED to clamp del_var_th(i) to the range [6, 600]"
static MIN_THRESHOLD: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(6));
static MAX_THRESHOLD: Lazy<Duration> = Lazy::new(|| Duration::milliseconds(600));

// From 5.5 ""Close" is defined as three standard deviations around this average"
const STANDARD_DEVIATION_CLOSE_NUM: f64 = 3.;

// Minimal duration between 2 updates on the lost based rate controller
static LOSS_UPDATE_INTERVAL: Lazy<time::Duration> = Lazy::new(|| time::Duration::from_millis(200));
static LOSS_DECREASE_THRESHOLD: f64 = 0.1;
static LOSS_INCREASE_THRESHOLD: f64 = 0.02;
static LOSS_INCREASE_FACTOR: f64 = 1.05;

// Minimal duration between 2 updates on the lost based rate controller
static DELAY_UPDATE_INTERVAL: Lazy<time::Duration> = Lazy::new(|| time::Duration::from_millis(100));

static ROUND_TRIP_TIME_WINDOW_SIZE: usize = 100;

fn ts2dur(t: gst::ClockTime) -> Duration {
    Duration::nanoseconds(t.nseconds() as i64)
}

#[derive(Debug)]
enum CongestionControlOp {
    /// Don't update target bitrate
    Hold,
    /// Decrease target bitrate
    Decrease {
        #[allow(dead_code)]
        reason: String, // for Debug
    },
    Increase,
}

#[derive(Debug, Clone, Copy)]
enum ControllerType {
    // Running the "delay-based controller"
    Delay,
    // Running the "loss based controller"
    Loss,
}

#[derive(Debug, Clone, Copy)]
struct Packet {
    departure: Duration,
    arrival: Duration,
    size: usize,
}

fn human_kbits<T: Into<f64>>(bits: T) -> String {
    format!("{:.2}kb", (bits.into() / 1_000.))
}

impl Packet {
    fn from_structure(structure: &gst::Structure) -> Option<Self> {
        let lost = structure.get::<bool>("lost").unwrap();
        let departure = match structure.get::<gst::ClockTime>("local-ts") {
            Err(e) => {
                gst::fixme!(CAT, "Got packet feedback without local-ts: {:?} - what does that mean?", e);
                return None;
            }
            Ok(ts) => ts,
        };

        if lost {
            return Some(Packet {
                arrival: Duration::zero(),
                departure: ts2dur(departure),
                size: structure.get::<u32>("size").unwrap() as usize,
            });
        }

        let arrival = structure.get::<gst::ClockTime>("remote-ts").unwrap();

        Some(Packet {
            arrival: ts2dur(arrival),
            departure: ts2dur(departure),
            size: structure.get::<u32>("size").unwrap() as usize,
        })
    }
}

#[derive(Debug, Clone)]
struct PacketGroup {
    packets: DI<Vec<Packet>>,
    departure: Duration,       // ms
    arrival: Option<Duration>, // ms
}

fn pdur(d: &Duration) -> String {
    let stdd = time::Duration::from_nanos(d.num_nanoseconds().unwrap().abs() as u64);

    format!("{}{stdd:?}", if d.lt(&Duration::zero()) { "-" } else { "" })
}

impl PacketGroup {
    fn new() -> Self {
        Self {
            packets: Default::default(),
            departure: Duration::zero(),
            arrival: None,
        }
    }

    fn add(&mut self, packet: Packet) {
        if self.departure.is_zero() {
            self.departure = packet.departure;
        }

        self.arrival = Some(
            self.arrival
                .map_or_else(|| packet.arrival, |v| v.max(packet.arrival)),
        );

        gst::trace!(
            CAT,
            "Adding {:?} {}",
            gst::ClockTime::from_nseconds(packet.departure.num_nanoseconds().unwrap() as u64),
            pdur(&(self.inter_departure_time_pkt(&packet) - *BURST_TIME))
        );
        self.packets.push(packet);
    }

    /// Returns the delta between self.arrival_time and @prev_group.arrival_time in ms
    // t(i) - t(i-1)
    fn inter_arrival_time(&self, prev_group: &Self) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        self.arrival.unwrap() - prev_group.arrival.unwrap()
    }

    fn inter_arrival_time_pkt(&self, next_pkt: &Packet) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        next_pkt.arrival - self.arrival.unwrap()
    }

    /// Returns the delta between self.departure_time and @prev_group.departure_time in ms
    // T(i) - T(i-1)
    fn inter_departure_time(&self, prev_group: &Self) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        self.departure - prev_group.departure
    }

    fn inter_departure_time_pkt(&self, next_pkt: &Packet) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        next_pkt.departure - self.departure
    }

    /// Returns the delta between intern arrival time and inter departure time in ms
    fn inter_delay_variation(&self, prev_group: &Self) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        self.inter_arrival_time(prev_group) - self.inter_departure_time(prev_group)
    }

    fn inter_delay_variation_pkt(&self, next_pkt: &Packet) -> Duration {
        // Should never be called if we haven't gotten feedback for all
        // contained packets
        self.inter_arrival_time_pkt(next_pkt) - self.inter_departure_time_pkt(next_pkt)
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum Usage {
    Normal,
    Over,
    Under,
}

struct DetectorInner {
    // Grouping fields
    group: PacketGroup,
    prev_group: Option<PacketGroup>,
    measure: Duration, // Delay variation measure

    // FIXME: This is not used but should be according 5.3:
    //
    // "`f_max = max {1/(inter_departure_time[T(j) - T(j-1))]}` for j in
    // i-K+1,...,i is the highest rate at which the last K packet groups have
    // been received and chi"
    last_inter_departure_times: Vec<Duration>,

    last_received_packets: VecDeque<Packet>,

    // Last loss update
    last_loss_update: Option<time::Instant>,
    // Moving average of the packet loss
    loss_average: f64,

    // Kalman filter fields
    gain: f64,
    measurement_uncertainty: f64, // var_v_hat(i-1)
    estimate_error: f64,          // e(i-1)
    estimate: Duration,           // m_hat(i-1)

    // Threshold fields
    threshold: Duration,
    last_threshold_update: Option<time::Instant>,
    num_deltas: i64,

    // Overuse related fields
    increasing_counter: u32,
    last_overuse_estimate: Duration,
    last_use_detector_update: time::Instant,
    increasing_duration: Duration,

    // round-trip-time estimations
    rtts: VecDeque<Duration>,
    clock: gst::Clock,

    // Current network usage state
    usage: Usage,

    updated_channel: Option<Sender<bool>>,
}

// Monitors packet loss and network overuse through because of delay
struct Detector {
    inner: Mutex<DetectorInner>,
}

impl Debug for Detector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.lock().unwrap();
        write!(
            f,
            "Usage: {:?}. Effective bitrate: {}ps - Measure: {} Estimate: {} threshold {} - overuse_estimate {}",
            inner.usage,
            human_kbits(inner.effective_bitrate()),
            pdur(&inner.measure),
            pdur(&inner.estimate),
            pdur(&inner.threshold),
            pdur(&inner.last_overuse_estimate),
        )
    }
}

impl Detector {
    fn new() -> Arc<Self> {
        let inner = DetectorInner {
            group: PacketGroup::new(),
            prev_group: Default::default(),
            measure: Duration::zero(),
            last_inter_departure_times: Vec::with_capacity(LAST_INTER_DEPARTURE_SIZE + 1),

            /* Smallish value to hold PACKETS_RECEIVED_WINDOW packets */
            last_received_packets: VecDeque::with_capacity(400),


            last_loss_update: None,
            loss_average: 0.,

            gain: 0.,
            measurement_uncertainty: 0.,
            estimate_error: INITIAL_ERROR_COVARIANCE,
            estimate: Duration::zero(),

            threshold: *INITIAL_DEL_VAR_TH,
            last_threshold_update: None,
            num_deltas: 0,

            last_use_detector_update: time::Instant::now(),
            increasing_counter: 0,
            last_overuse_estimate: Duration::zero(),
            increasing_duration: Duration::zero(),

            rtts: Default::default(),
            clock: gst::SystemClock::obtain(),

            usage: Usage::Normal,

            updated_channel: None,
        };

        Arc::new(Self { inner: Mutex::new(inner) })
    }

    fn set_channel(&self, channel: Sender::<bool>) {
        self.inner.lock().unwrap().updated_channel = Some(channel);
    }

    fn rtt(&self) -> Duration {
        self.inner.lock().unwrap().rtt()
    }

    fn effective_bitrate(&self) -> Bitrate {
        let mut inner = self.inner.lock().unwrap();

        let len = inner.last_received_packets.len();
        if len == 0 {
            return 0;
        }

        inner.evict_old_received_packets();
        inner.effective_bitrate()
    }

    fn usage(&self) -> Usage {
        self.inner.lock().unwrap().usage
    }

    fn update(&self, packets: &Vec<Packet>) {
        self.inner.lock().unwrap().update(packets)
    }

    fn loss_ratio(&self) -> f64 {
        self.inner.lock().unwrap().loss_average
    }
}

impl DetectorInner {
    fn update_last_received_packets(&mut self, packet: Packet) {
        let arrival = packet.arrival;
        let idx = match self
            .last_received_packets
            .binary_search_by(|packet| packet.arrival.cmp(&arrival))
        {
            Ok(idx) => idx,
            Err(idx) => idx,
        };
        self.last_received_packets.insert(idx, packet);
    }

    fn evict_old_received_packets(&mut self) {
        let last_arrival = self
            .last_received_packets
            .get(self.last_received_packets.len() - 1)
            .unwrap()
            .arrival;

        while last_arrival - self.oldest_packet_in_window_ts() > *PACKETS_RECEIVED_WINDOW {
            self.last_received_packets.remove(0);
        }
    }

    /// Returns the effective received bitrate during the last PACKETS_RECEIVED_WINDOW
    fn effective_bitrate(&self) -> Bitrate {
        let len = self.last_received_packets.len();
        if len == 0 {
            return 0;
        }

        let duration = self.last_received_packets.get(len - 1).unwrap().arrival
            - self.last_received_packets.get(0).unwrap().arrival;
        let bits = self
            .last_received_packets
            .iter()
            .map(|p| p.size as f64)
            .sum::<f64>()
            * 8.;

        (bits / (duration.num_nanoseconds().unwrap() as f64
            / gst::ClockTime::SECOND.nseconds() as f64)) as Bitrate
    }

    fn oldest_packet_in_window_ts(&self) -> Duration{
        self.last_received_packets.get(0).unwrap().arrival
    }

    fn update_rtts(&mut self, packets: &Vec<Packet>) {
        let mut rtt = Duration::nanoseconds(i64::MAX);
        let now = ts2dur(self.clock.time().unwrap());
        for packet in packets {
            rtt = (now - packet.departure).min(rtt);
        }

        self.rtts.push_back(rtt);
        if self.rtts.len() > ROUND_TRIP_TIME_WINDOW_SIZE {
            self.rtts.pop_front();
        }
    }

    fn rtt(&self) -> Duration {
        Duration::nanoseconds(
            (self.rtts.iter().map(|d| d.num_nanoseconds().unwrap() as f64).sum::<f64>() / self.rtts.len() as f64) as i64
        )
    }

    fn update(&mut self, packets: &Vec<Packet>) {
        self.update_rtts(packets);
        let mut lost_packets = 0.;
        for pkt in packets {
            if pkt.arrival.is_zero() {
                lost_packets += 1.;
                continue;
            }

            self.update_last_received_packets(pkt.clone());

            if self.group.arrival.is_none() {
                gst::log!(
                    CAT,
                    "Starting with {:?}",
                    gst::ClockTime::from_nseconds(pkt.departure.num_nanoseconds().unwrap() as u64)
                );
                self.group.add(*pkt);

                continue;
            }

            if pkt.arrival < self.group.arrival.unwrap() {
                gst::debug!(
                    CAT,
                    "Ignoring with {:?}",
                    gst::ClockTime::from_nseconds(pkt.departure.num_nanoseconds().unwrap() as u64)
                );
                // ignore out of order arrivals
                continue;
            }

            if pkt.departure >= self.group.departure {
                if self.group.inter_departure_time_pkt(&pkt) < *BURST_TIME {
                    self.group.add(*pkt);
                    continue;
                }

                if self.group.inter_arrival_time_pkt(&pkt) < *BURST_TIME
                    && self.group.inter_delay_variation_pkt(&pkt) < Duration::zero()
                {
                    self.group.add(*pkt);
                    continue;
                }

                let group = mem::replace(&mut self.group, PacketGroup::new());
                gst::trace!(
                    CAT,
                    "Packet done: {:?}",
                    gst::ClockTime::from_nseconds(group.departure.num_nanoseconds().unwrap() as u64)
                );
                if let Some(prev_group) = mem::replace(&mut self.prev_group, Some(group.clone())) {
                    self.last_inter_departure_times
                        .push(group.inter_departure_time(&prev_group));
                    if self.last_inter_departure_times.len() > LAST_INTER_DEPARTURE_SIZE {
                        self.last_inter_departure_times.remove(0);
                    }
                    self.kalman_estimate(&prev_group, &group);
                    self.overuse_filter();

                }
            } else {
                gst::debug!(
                    CAT,
                    "Ignoring with {:?}",
                    gst::ClockTime::from_nseconds(pkt.departure.num_nanoseconds().unwrap() as u64)
                );
            }
        }

        self.compute_loss_average(lost_packets as f64 / packets.len() as f64);
        self.evict_old_received_packets();

        if let Some(ref mut updated_channel) = self.updated_channel.as_mut() {
            task::block_on(async {
                if let Err(err) = updated_channel.send(true).await {
                    gst::error!(CAT, "Error updating bitrate? {err:?}");
                }
            });
        }
    }

    fn compute_loss_average(&mut self, loss_fraction: f64) {
        let now = time::Instant::now();

        if let Some(ref last_update) = self.last_loss_update {
            self.loss_average = loss_fraction +
                (-Duration::from_std(now - *last_update).unwrap().num_milliseconds() as f64).exp() * (self.loss_average - loss_fraction);
        }

        self.last_loss_update = Some(now);
    }

    fn kalman_estimate(&mut self, prev_group: &PacketGroup, group: &PacketGroup) {
        self.measure = group.inter_delay_variation(prev_group);

        let z = self.measure - self.estimate;
        let zms = z.num_microseconds().unwrap() as f64 / 1000.0;

        // FIXME: Where is my f_max?
        let alpha = ONE_MINUS_CHI.powf(30.0 / (1000. * 5. * 1_000_000.));
        let root = self.measurement_uncertainty.sqrt();
        let root3 = 3. * root;

        if zms > root3 {
            self.measurement_uncertainty =
                (alpha * self.measurement_uncertainty + (1. - alpha) * root3 * root3).max(1.);
        }
        self.measurement_uncertainty =
            (alpha * self.measurement_uncertainty + (1. - alpha) * zms * zms).max(1.);
        let estimate_uncertainty = self.estimate_error + Q;
        self.gain = estimate_uncertainty / (estimate_uncertainty + self.measurement_uncertainty);
        self.estimate =
            self.estimate + Duration::nanoseconds((self.gain * zms * 1_000_000.) as i64);
        self.estimate_error = (1. - self.gain) * estimate_uncertainty;
    }

    fn compare_threshold(&mut self) -> (Usage, Duration) {
        const MAX_DELTAS: i64 = 30;

        self.num_deltas += 1;
        if self.num_deltas < 2 {
            return (Usage::Normal, self.estimate);
        }

        // FIXME: WHere is that delta count comming from?
        let t = Duration::nanoseconds(
            self.estimate.num_nanoseconds().unwrap() * self.num_deltas.min(MAX_DELTAS),
        );
        let usage = if t > self.threshold {
            Usage::Over
        } else if t.num_nanoseconds().unwrap() < -self.threshold.num_nanoseconds().unwrap() {
            Usage::Under
        } else {
            Usage::Normal
        };

        self.update_threshold(&t);

        (usage, t)
    }

    fn update_threshold(&mut self, estimate: &Duration) {
        const K_U: f64 = 0.01; // Table1. Coefficient for the adaptive threshold
        const K_D: f64 = 0.00018; // Table1. Coefficient for the adaptive threshold
        const MAX_TIME_DELTA: time::Duration = time::Duration::from_millis(100);

        let now = time::Instant::now();
        if self.last_threshold_update.is_none() {
            self.last_threshold_update = Some(now);
        }

        let abs_estimate = Duration::nanoseconds(estimate.num_nanoseconds().unwrap().abs());
        if abs_estimate > self.threshold + *MAX_M_MINUS_DEL_VAR_TH {
            self.last_threshold_update = Some(now);
            return;
        }

        let k = if abs_estimate < self.threshold {
            K_D
        } else {
            K_U
        };
        let time_delta =
            Duration::from_std((now - self.last_threshold_update.unwrap()).min(MAX_TIME_DELTA))
                .unwrap();
        let d = abs_estimate - self.threshold;
        let add = k * d.num_milliseconds() as f64 * time_delta.num_milliseconds() as f64;

        self.threshold = self.threshold + Duration::nanoseconds((add * 100. * 1_000.) as i64);
        self.threshold = self.threshold.clamp(*MIN_THRESHOLD, *MAX_THRESHOLD);
        self.last_threshold_update = Some(now);
    }

    fn overuse_filter(&mut self) {
        let (th_usage, estimate) = self.compare_threshold();

        let now = time::Instant::now();
        let delta = Duration::from_std(now - self.last_use_detector_update).unwrap();
        self.last_use_detector_update = now;
        gst::log!(
            CAT,
            "{:?} - self.estimate {} - estimate: {} - th: {}",
            th_usage,
            pdur(&self.estimate),
            pdur(&estimate),
            pdur(&self.threshold)
        );
        match th_usage {
            Usage::Over => {
                self.increasing_duration = self.increasing_duration + delta;
                self.increasing_counter += 1;

                if self.increasing_duration > *OVERUSE_TIME_TH
                    && self.increasing_counter > 1
                    && estimate > self.last_overuse_estimate
                {
                    self.usage = Usage::Over;
                }
            }
            Usage::Under | Usage::Normal => {
                self.increasing_duration = Duration::zero();
                self.increasing_counter = 0;

                self.usage = th_usage;
            }
        }
        self.last_overuse_estimate = estimate;
    }
}

#[derive(Debug)]
struct PacerInner {
    #[allow(dead_code)]
    probe_id: Option<gst::PadProbeId>,

    buffers: DI<VecDeque<gst::Buffer>>,
    sinkpad: Option<gst::Pad>,
    bitrate: Bitrate,
    task: Option<task::JoinHandle<()>>,
}

#[derive(Debug)]
struct Pacer {
    inner: Arc<Mutex<PacerInner>>,
    detector: Arc<Detector>,
}

impl Pacer {
    fn new(detector: Arc<Detector>) -> Self {
        let inner = Arc::new(Mutex::new(PacerInner {
            probe_id: None,
            buffers: DI(Default::default()),
            bitrate: DEFAULT_START_BITRATE,
            sinkpad: None,
            task: None,
        }));

        let this = Self { inner, detector };

        this
    }

    fn start(&self, webrtcbin: &gst::Element) {
        self.collect_buffers(webrtcbin);
        self.start_task();
    }

    fn bitrate(&mut self) -> Bitrate {
        self.inner.lock().unwrap().bitrate
    }


    fn set_bitrate(&mut self, bitrate: Bitrate) {
        self.inner.lock().unwrap().bitrate = bitrate;
    }

    fn collect_buffers(&self, webrtcbin: &gst::Element) {
        let deep_element_added_sigid: Arc<AtomicRefCell<Option<glib::SignalHandlerId>>> =
            Default::default();
        let inner = self.inner.clone();
        let detector = self.detector.clone();

        let rtpbin = webrtcbin
            .dynamic_cast_ref::<gst::ChildProxy>()
            .unwrap()
            .child_by_name("rtpbin")
            .unwrap()
            .downcast::<gst::Element>()
            .unwrap();
        let _ = deep_element_added_sigid.borrow_mut().insert(
            rtpbin.connect_pad_added(
                    glib::clone!(@strong deep_element_added_sigid, @weak inner, @weak detector => move |rtpbin, pad| {
                let rtpbin = rtpbin.clone().downcast::<gst::Bin>().unwrap();

                let pad_name = pad.name();
                let splitres = pad_name.split("send_rtp_sink_");

                if splitres.count() != 2 {
                    return;
                }

                // FIXME: Working around https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/2411
                // let session = rtpbin.emit_byt_name("get-session", splites.get(1));
                let session = rtpbin.iterate_all_by_element_factory_name("rtpsession").into_iter().next().unwrap().unwrap();
                let _ = deep_element_added_sigid.borrow_mut().take();

                let sinkpad = session.static_pad("send_rtp_sink").unwrap();
                let (weak_detector, weak_inner) = (detector.downgrade(), inner.downgrade());
                if let Some(probe_id) = sinkpad.add_probe(
                        gst::PadProbeType::EVENT_UPSTREAM | gst::PadProbeType::BUFFER, move |_pad, info| {
                            let detector = weak_detector.upgrade().unwrap();
                            let inner = weak_inner.upgrade().unwrap();

                            if info.mask.contains(gst::PadProbeType::BUFFER) {
                                if let gst::PadProbeData::Buffer(buffer) = info.data.take().unwrap() {
                                    inner.lock().unwrap().buffers.push_front(buffer);
                                }

                                gst::PadProbeReturn::Handled

                            } else {
                                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                                    if let Some(structure) = event.structure() {
                                        if structure.name() == "RTPTWCCPackets" {
                                            let varray = structure.get::<glib::ValueArray>("packets").unwrap();
                                            let packets = varray.iter().filter_map(|s| Packet::from_structure(&s.get::<gst::Structure>().unwrap())).collect::<Vec<Packet>>();
                                            detector.update(&packets);
                                        }
                                    }
                                }

                                gst::PadProbeReturn::Ok
                            }
                        }
                    ) {
                    let mut lthis = inner.lock().unwrap();
                    lthis.probe_id.replace(probe_id);
                    lthis.sinkpad.replace(sinkpad);
                }
            })
        ));
    }

    fn start_task(&self) {
        let inner = self.inner.clone();
        let detector = self.detector.clone();

        let mut last_sent = time::Instant::now();
        inner.lock().unwrap().task = Some(task::spawn(
            glib::clone!(@weak-allow-none inner, @weak-allow-none detector => async move {
                let mut interval =
                    async_std::stream::interval(BURST_TIME.to_std().unwrap());

                while interval.next().await.is_some() {
                    if let Some(ref inner) =  &inner {
                        let mut inner = inner.lock().unwrap();
                        if inner.sinkpad.is_some() {
                            let now = time::Instant::now();
                            let elapsed = Duration::from_std(now - last_sent).unwrap();
                            let mut budget = (elapsed.num_nanoseconds().unwrap()).mul_div_round(inner.bitrate as i64, gst::ClockTime::SECOND.nseconds() as i64).unwrap();
                            let total_budget = budget;
                            let mut remaining = inner.buffers.iter().map(|b| b.size() as f64).sum::<f64>() * 8.;
                            let total_size = remaining;
                            gst::log!(CAT, "{} Size: {} - Budget: {} - target bitrate: {}ps", pdur(&elapsed), human_kbits(remaining), human_kbits(budget as f64), human_kbits(inner.bitrate));

                            let mut list = gst::BufferList::new();
                            let mutlist = list.get_mut().unwrap();

                            // Leak the bucket so it can hold at most 30ms of data
                            let maximum_remaining_bits = 30. * inner.bitrate as f64 / 1000.;
                            while (budget > 0 || remaining > maximum_remaining_bits) && !inner.buffers.is_empty() {
                                let buf = inner.buffers.pop_back().unwrap();
                                let n_bits = buf.size() * 8;

                                mutlist.add(buf);
                                budget = budget.saturating_sub(n_bits as i64);
                                remaining = remaining - n_bits as f64;
                            }

                            gst::log!(CAT, "{} bitrate: {}ps budget: {}/{} sending: {} Remaining: {}/{}",
                                pdur(&elapsed),
                                human_kbits(inner.bitrate),
                                human_kbits(budget as f64),
                                human_kbits(total_budget as f64),
                                human_kbits(list.calculate_size() as f64 * 8.),
                                human_kbits(remaining),
                                human_kbits(total_size)
                            );

                            let sinkpad = inner.sinkpad.as_ref().unwrap().clone();
                            drop(inner);

                            if list.len() > 0 {
                                let _ = sinkpad.chain_list(list);
                            }
                            last_sent = now;
                        }
                    } else {
                        break;
                    }
                }
            }),
        ));
    }

    async fn stop(&self) {
        if let Some(task) = self.inner.lock().unwrap().task.take() {
            task.cancel().await;
        }
    }
}

#[derive(Default, Debug)]
struct ExponentialMovingAverage {
    average: f64,
    variance: f64,
    standard_dev: f64,
}

impl ExponentialMovingAverage {
    fn update<T: Into<f64>>(&mut self, value: T) {
        if self.average == 0.0 {
            self.average = value.into();
        } else {
            let x = value.into() - self.average;

            self.average += MOVING_AVERAGE_SMOOTHING_FACTOR * x;
            self.variance = (1. - MOVING_AVERAGE_SMOOTHING_FACTOR)
                * (self.variance + MOVING_AVERAGE_SMOOTHING_FACTOR * x * x);
            self.standard_dev = self.variance.sqrt();
        }
    }

    fn estimate_is_close(&self, value: Bitrate) -> bool {
        if self.average == 0. {
            false
        } else {
            ((self.average - STANDARD_DEVIATION_CLOSE_NUM * self.standard_dev)
                ..(self.average + STANDARD_DEVIATION_CLOSE_NUM * self.standard_dev))
                .contains(&(value as f64))
        }
    }
}

pub struct RateControllerInner {
    /// Note: The target bitrate applied is the min of
    /// target_bitrate_on_delay and target_bitrate_on_loss
    ///
    /// Bitrate target based on delay factor for all video streams.
    /// Hasn't been tested with multiple video streams, but
    /// current design is simply to divide bitrate equally.
    target_bitrate_on_delay: Bitrate,

    /// Used in additive mode to track last control time, influences
    /// calculation of added value according to gcc section 5.5
    last_increase_on_delay: Option<time::Instant>,
    last_decrease_on_delay: time::Instant,

    /// Bitrate target based on loss for all video streams.
    target_bitrate_on_loss: Bitrate,

    last_increase_on_loss: time::Instant,
    last_decrease_on_loss: time::Instant,

    /// Exponential moving average, updated when bitrate is
    /// decreased
    ema: ExponentialMovingAverage,

    state: CongestionControlOp,

    min_bitrate: Bitrate,
    max_bitrate: Bitrate,

    control_task: Option<task::JoinHandle<()>>,

    detector: Arc<Detector>,
    pacer: Pacer,
}

#[glib::object_subclass]
impl ObjectSubclass for RateController {
    const NAME: &'static str = "GoogleCongestionController";
    type Type = super::RateController;
    type ParentType = gst::Object;
}

impl GstObjectImpl for RateController {}

impl ObjectImpl for RateController {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                /*
                 *  gcc:bitrate:
                 *
                 * Currently computed network bitrate, should be used
                 * to set encoders bitrate.
                 */
                glib::ParamSpecUInt::new(
                    "bitrate",
                    "Bitrate",
                    "Currently computed network bitrate.",
                    1,
                    u32::MAX as u32,
                    DEFAULT_MIN_BITRATE as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }
}

impl Default for RateControllerInner {
    fn default() -> Self {
        let detector = Detector::new();

        Self {
            target_bitrate_on_delay: DEFAULT_START_BITRATE,
            target_bitrate_on_loss: DEFAULT_START_BITRATE,
            last_increase_on_loss: time::Instant::now(),
            last_decrease_on_loss: time::Instant::now(),
            ema: Default::default(),
            last_increase_on_delay: None,
            last_decrease_on_delay: time::Instant::now(),
            min_bitrate: DEFAULT_MIN_BITRATE,
            max_bitrate: DEFAULT_MAX_BITRATE,
            detector: detector.clone(),
            pacer: Pacer::new(detector),
            state: CongestionControlOp::Increase,
            control_task: None,
        }
    }
}

impl RateControllerInner {
    fn compute_increased_rate(&mut self, controller: &super::RateController) -> Option<Bitrate> {
        let now = time::Instant::now();
        let target_bitrate = self.target_bitrate_on_delay as f64;
        let effective_bitrate = self.detector.effective_bitrate();
        let time_since_last_update_ms = match self.last_increase_on_delay {
            None => 0.,
            Some(prev) => {
                if now - prev < *DELAY_UPDATE_INTERVAL {
                    return None;
                }

                (now - prev).as_millis() as f64
            }
        };

        self.last_increase_on_delay = Some(now);
        if self.ema.estimate_is_close(effective_bitrate) {
            let bits_per_frame = target_bitrate / 30.;
            let packets_per_frame = f64::ceil(bits_per_frame / (1200. * 8.));
            let avg_packet_size_bits = bits_per_frame / packets_per_frame;

            let rtt_ms = self.detector.rtt().num_milliseconds() as f64;
            let response_time_ms = 100. + rtt_ms;
            let alpha = 0.5 * f64::min(time_since_last_update_ms / response_time_ms, 1.0);
            let increase = f64::max(1000.0, alpha * avg_packet_size_bits);

            /* Additive increase */
            Some(f64::min(
                self.target_bitrate_on_delay as f64 + increase,
                1.5 * effective_bitrate as f64,
            ) as Bitrate)
        } else {
            let eta = 1.08_f64.powf(f64::min(time_since_last_update_ms / 1000., 1.0));
            let rate = eta * self.target_bitrate_on_delay as f64;

            // Maximum increase to 1.5 * received rate
            let received_max = 1.5 * effective_bitrate as f64;

            if rate > received_max && received_max > self.target_bitrate_on_delay as f64 {
                gst::log!(
                    CAT,
                    obj: controller,
                    "Increasing == received_max rate: {}ps",
                    human_kbits(received_max)
                );

                Some(received_max as Bitrate)
            } else if rate < self.target_bitrate_on_delay as f64 {
                gst::log!(
                    CAT,
                    obj: controller,
                    "Rate < target, returning {}ps",
                    human_kbits(self.target_bitrate_on_delay)
                );

                None
            } else {
                gst::log!(
                    CAT,
                    obj: controller,
                    "Increase mult {eta}x{}ps={}ps",
                    human_kbits(self.target_bitrate_on_delay),
                    human_kbits(rate)
                );

                Some(rate as Bitrate)
            }
        }
    }

    fn set_bitrate(
        &mut self,
        controller: &super::RateController,
        bitrate: Bitrate,
        controller_type: ControllerType,
    ) -> bool {
        let prev_bitrate = Bitrate::min(self.target_bitrate_on_delay, self.target_bitrate_on_loss);

        match controller_type {
            ControllerType::Loss => {
                self.target_bitrate_on_loss = bitrate.clamp(self.min_bitrate, self.max_bitrate)
            }

            ControllerType::Delay => {
                self.target_bitrate_on_delay = bitrate.clamp( self.min_bitrate, self.max_bitrate)
            }
        }

        let target_bitrate =
            Bitrate::min(self.target_bitrate_on_delay, self.target_bitrate_on_loss).clamp(
                self.min_bitrate,
                self.max_bitrate,
            );

        if target_bitrate == prev_bitrate {
            return false;
        }

        gst::info!(
            CAT,
            obj: controller,
            "{controller_type:?}: {}ps => {}ps ({:?})",
            human_kbits(prev_bitrate),
            human_kbits(target_bitrate),
            self.state,
        );

        self.pacer.set_bitrate(target_bitrate);

        true
    }

    pub fn loss_control(
        &mut self,
        controller: &super::RateController,
    ) -> bool {
        let loss_ratio = self.detector.loss_ratio();
        let now = time::Instant::now();

        if loss_ratio > LOSS_DECREASE_THRESHOLD && (now - self.last_decrease_on_loss) > *LOSS_UPDATE_INTERVAL {
            let factor = 1. - (0.5 * loss_ratio);

            self.state = CongestionControlOp::Decrease {
                reason: format!("High loss detected ({loss_ratio:2}"),
            };
            self.last_decrease_on_loss = now;

            self.set_bitrate(
                controller,
                (self.target_bitrate_on_loss as f64 * factor) as Bitrate,
                ControllerType::Loss,
            )
        } else if loss_ratio < LOSS_INCREASE_THRESHOLD && (now - self.last_increase_on_loss) > *LOSS_UPDATE_INTERVAL {
            self.state = CongestionControlOp::Increase;
            self.last_increase_on_loss = now;

            self.set_bitrate(
                controller,
                (self.target_bitrate_on_loss as f64 * LOSS_INCREASE_FACTOR) as Bitrate,
                ControllerType::Loss,
            )
        } else {
            false
        }
    }

    pub fn delay_control(
        &mut self,
        controller: &super::RateController,
    ) -> bool {
        match self.detector.usage() {
            Usage::Normal => {
                match self.state {
                    CongestionControlOp::Increase |  CongestionControlOp::Hold => {
                        if let Some(bitrate) = self.compute_increased_rate(controller) {
                            self.state = CongestionControlOp::Increase;
                            return self.set_bitrate(controller, bitrate, ControllerType::Delay);
                        }
                    }
                    _ => ()
                }
            }
            Usage::Over => {
                let now = time::Instant::now();
                if now - self.last_decrease_on_delay > *DELAY_UPDATE_INTERVAL {
                    let effective_bitrate = self.detector.effective_bitrate();
                    let target = (self.pacer.bitrate() as f64 * 0.95).min((BETA * effective_bitrate as f64).into());
                    self.state = CongestionControlOp::Decrease {
                        reason: format!("Over use detected {:#?}", self.detector)
                    };
                    self.ema.update(effective_bitrate);
                    self.last_decrease_on_delay = now;

                    return self.set_bitrate(controller, target as Bitrate, ControllerType::Delay);
                }
            }
            Usage::Under => {
                if let CongestionControlOp::Increase = self.state {
                    if let Some(bitrate) = self.compute_increased_rate(controller) {
                        return self.set_bitrate(controller, bitrate, ControllerType::Delay);
                    }
                }
            }
        }

        self.state = CongestionControlOp::Hold;

        false
    }

    pub fn start(
        &mut self,
        controller: &super::RateController,
        webrtcbin: &gst::Element,
        min_bitrate: Bitrate,
        start_bitrate: Bitrate,
        max_bitrate: Bitrate,
    ) {
        self.max_bitrate = max_bitrate;
        self.min_bitrate = min_bitrate;
        self.target_bitrate_on_delay = start_bitrate;
        self.target_bitrate_on_loss = start_bitrate;
        self.pacer.set_bitrate(start_bitrate);
        self.pacer.start(webrtcbin);

        let (sender, mut receiver) = futures::channel::mpsc::channel::<bool>(1000);
        self.detector.set_channel(sender);

        let weak_controller = ObjectExt::downgrade(controller);
        self.control_task = Some(task::spawn(async move {
            while let Some(_) = receiver.next().await {
                if let Some(ref controller) = &weak_controller.upgrade() {
                    let this = controller.imp();
                    let mut inner = this.inner.lock().unwrap();
                    let mut notify = inner.delay_control(controller);
                    if !notify {
                        notify = inner.loss_control(controller);
                    }
                    drop(inner);

                    if notify {
                        controller.notify("bitrate")
                    }
                } else {
                    break;
                }

            }
        }));
    }

    pub fn stop(&mut self) {
        // FIXME: Avoid blocking here?
        task::block_on(async {
            if let Some(control_task) = self.control_task .take(){
                control_task.cancel().await;
            }
            self.pacer.stop().await;
        });
    }
}

#[derive(Default)]
pub struct RateController {
    inner: Mutex<RateControllerInner>,
}

impl RateController {
    pub fn start(
        &self,
        controller: &super::RateController,
        webrtcbin: &gst::Element,
        min_bitrate: Bitrate,
        start_bitrate: Bitrate,
        max_bitrate: Bitrate,
    ) {
        self.inner.lock().unwrap().start(controller, webrtcbin, min_bitrate, start_bitrate, max_bitrate);
    }

    pub fn stop(&self) {
        self.inner.lock().unwrap().stop();
    }

    pub fn bitrate(&self) -> i32 {
        self.inner.lock().unwrap().pacer.bitrate() as i32
    }
}
