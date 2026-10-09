//! One plugin instance, hosted in this process: the component/controller pair (driven from the UI
//! thread) and its [`Processor`] (driven from the audio thread).
//!
//! Lifecycle, as the VST3 hosting guide describes it: create the component, initialise it with the
//! host context, find or create the edit controller, connect the two, copy the component state to
//! the controller, choose bus arrangements, `setupProcessing`, `setActive(true)`; then on the audio
//! thread `setProcessing(true)` and `process` per block. Teardown runs the same steps in reverse.

use crate::com::{note_off, note_on, ComponentHandler, EventList, HandlerEvent, HandlerSink, HostApplication, MemStream, ParamChanges};
use crate::module::{ClassInfo, FactoryInfo, Module};
use crate::util::{check, parse_cid, result_name, str16, wide};
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use vst3::Steinberg::Vst::BusDirections_::{kInput, kOutput};
use vst3::Steinberg::Vst::BusTypes_::kMain;
use vst3::Steinberg::Vst::MediaTypes_::{kAudio, kEvent};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};

/// Most events / parameter changes one block can carry.
pub const MAX_EVENTS: usize = 2048;
pub const MAX_PARAM_QUEUES: usize = 512;
pub const MAX_POINTS: usize = 64;
/// MIDI controller numbers the host maps to parameters: CC 0-127, channel aftertouch (128), pitch bend (129).
const CTRL_COUNT: usize = 130;

/// How the plugin will be run.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Setup {
    pub sample_rate: f64,
    /// Largest block `process` will be called with.
    pub max_block: usize,
    /// Offline (bounce) rendering: plugins may use higher quality and needn't keep up with realtime.
    pub offline: bool,
}

/// A parameter as the controller describes it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ParamInfo {
    pub id: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub short_title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub units: String,
    /// 0 = continuous; n = n+1 discrete values.
    pub step_count: i32,
    pub default: f64,
    pub unit_id: i32,
    /// ParameterFlags bits.
    pub flags: i32,
}

impl ParamInfo {
    pub fn automatable(&self) -> bool {
        self.flags & ParameterInfo_::ParameterFlags_::kCanAutomate as i32 != 0
    }
    pub fn read_only(&self) -> bool {
        self.flags & ParameterInfo_::ParameterFlags_::kIsReadOnly as i32 != 0
    }
    pub fn hidden(&self) -> bool {
        self.flags & ParameterInfo_::ParameterFlags_::kIsHidden as i32 != 0
    }
    pub fn is_list(&self) -> bool {
        self.flags & ParameterInfo_::ParameterFlags_::kIsList as i32 != 0
    }
    pub fn program_change(&self) -> bool {
        self.flags & ParameterInfo_::ParameterFlags_::kIsProgramChange as i32 != 0
    }
    pub fn bypass(&self) -> bool {
        self.flags & ParameterInfo_::ParameterFlags_::kIsBypass as i32 != 0
    }
}

/// An audio bus and how we configured it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Bus {
    pub name: String,
    pub channels: usize,
    pub main: bool,
    pub active: bool,
}

/// What a running instance looks like.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PluginInfo {
    pub class: ClassInfo,
    pub factory: FactoryInfo,
    pub inputs: Vec<Bus>,
    pub outputs: Vec<Bus>,
    pub event_inputs: usize,
    pub latency: u32,
    pub tail: u32,
    pub params: usize,
    /// Processor and controller are one object (no separate controller class).
    pub single_component: bool,
    pub setup: Setup,
}

/// A named list of programs (factory presets) the plugin exposes through `IUnitInfo`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProgramList {
    pub id: i32,
    pub name: String,
    pub unit_id: i32,
    pub unit_name: String,
    pub programs: Vec<String>,
    /// The parameter that selects a program in this unit (normalized = index / (count - 1)), if any.
    pub param: Option<u32>,
}

/// The plugin's saved state: what `IComponent::getState` and `IEditController::getState` wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub component: Vec<u8>,
    pub controller: Vec<u8>,
}

/// A MIDI-style event at a sample offset within the block.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimedMidi {
    pub offset: u32,
    pub msg: Midi,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Midi {
    NoteOn { ch: u8, pitch: u8, vel: u8 },
    NoteOff { ch: u8, pitch: u8, vel: u8 },
    Cc { ch: u8, cc: u8, value: u8 },
    /// -8192..=8191
    PitchBend { ch: u8, value: i16 },
    ChannelPressure { ch: u8, value: u8 },
}

/// A sample-accurate parameter change.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamPoint {
    pub id: u32,
    pub offset: u32,
    /// Normalized 0..1.
    pub value: f64,
}

/// Musical position for tempo-synced plugins.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transport {
    pub playing: bool,
    pub tempo: f64,
    pub ts_num: i32,
    pub ts_den: i32,
    /// Song position of the block's first sample.
    pub project_samples: i64,
    /// Same position in quarter notes.
    pub ppq: f64,
    /// Quarter-note position of the bar the block starts in.
    pub bar_ppq: f64,
    /// Loop range in quarter notes while cycling.
    pub cycle: Option<(f64, f64)>,
}

impl Default for Transport {
    fn default() -> Self {
        Transport { playing: false, tempo: 120.0, ts_num: 4, ts_den: 4, project_samples: 0, ppq: 0.0, bar_ppq: 0.0, cycle: None }
    }
}

/// What happened in a `process` call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessReport {
    /// The block was skipped (reconfiguring, terminated or the plugin failed); outputs are silent.
    pub skipped: bool,
    /// MIDI messages the plugin has no parameter mapping for (CC/pitch bend without IMidiMapping).
    pub unmapped: u32,
    /// Events that didn't fit the block's lists.
    pub dropped: u32,
}

/// State shared between the UI-thread object, the audio-thread processor and the handler callbacks.
struct Shared {
    /// Parameter values to hand the processor in its next block (edits from the UI or the host).
    pending: Mutex<Vec<(u32, f64)>>,
    /// Values the processor reported (outputParameterChanges) for the controller.
    reported: Mutex<Vec<(u32, f64)>>,
    restart: AtomicI32,
    dirty: AtomicBool,
    /// Held by the processor while it processes; the UI thread holds it to reconfigure.
    process_lock: Mutex<()>,
    /// The processor must call setProcessing(true) again before its next block.
    reprocess: AtomicBool,
    terminated: AtomicBool,
    latency: AtomicU32,
    midi_map: Mutex<Arc<MidiMap>>,
    /// Bumps when the midi map changes.
    midi_map_rev: AtomicU32,
    /// Channels per input bus, per output bus, and the active main input (bus layout for the processor).
    layout: Mutex<Layout>,
    /// Bumps when the bus layout changes (kIoChanged).
    layout_rev: AtomicU32,
    listener: Mutex<Option<HandlerSink>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Layout {
    inputs: Vec<usize>,
    outputs: Vec<usize>,
    main_in: Option<usize>,
}

/// controller number -> parameter, per channel.
#[derive(Clone)]
pub struct MidiMap {
    map: Vec<Option<u32>>,
}

impl MidiMap {
    fn empty() -> MidiMap {
        MidiMap { map: vec![None; 16 * CTRL_COUNT] }
    }

    pub fn get(&self, ch: u8, ctrl: usize) -> Option<u32> {
        if ctrl >= CTRL_COUNT {
            return None;
        }
        self.map[(ch as usize & 15) * CTRL_COUNT + ctrl]
    }

    /// How many controllers are mapped on channel 0 (for diagnostics).
    pub fn mapped(&self) -> usize {
        self.map[..CTRL_COUNT].iter().filter(|m| m.is_some()).count()
    }
}

/// The UI-thread side of a plugin instance.
pub struct Plugin {
    // COM pointers first: fields drop in order, and the module (last) must outlive them.
    connection: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    controller: Option<ComPtr<IEditController>>,
    processor: ComPtr<IAudioProcessor>,
    component: ComPtr<IComponent>,
    handler: ComWrapper<ComponentHandler>,
    host: ComWrapper<HostApplication>,
    controller_separate: bool,
    active: bool,
    setup: Setup,
    class: ClassInfo,
    inputs: Vec<Bus>,
    outputs: Vec<Bus>,
    event_inputs: usize,
    shared: Arc<Shared>,
    processor_taken: bool,
    module: Arc<Module>,
}

// SAFETY: VST3 objects are called from the thread that owns the Plugin (the UI thread); it is only moved
// between threads before use (e.g. constructed then handed to the UI thread).
unsafe impl Send for Plugin {}

impl Plugin {
    /// Instantiate the processor class `cid` (32 hex digits; None = the module's first "Audio Module Class").
    pub fn new(module: Arc<Module>, cid: Option<&str>, setup: Setup) -> Result<Plugin> {
        if !(8000.0..=768_000.0).contains(&setup.sample_rate) {
            bail!("unsupported sample rate {}", setup.sample_rate);
        }
        if setup.max_block == 0 || setup.max_block > 65536 {
            bail!("unsupported block size {}", setup.max_block);
        }
        let classes = module.classes()?;
        let class = match cid {
            Some(c) => classes
                .iter()
                .find(|k| k.cid.eq_ignore_ascii_case(c) || k.name == c)
                .cloned()
                .ok_or_else(|| anyhow!("{} has no class '{c}'", module.paths.binary.display()))?,
            None => classes
                .iter()
                .find(|k| k.is_processor())
                .cloned()
                .ok_or_else(|| anyhow!("{} exports no audio processor", module.paths.binary.display()))?,
        };
        let host = HostApplication::new();
        let host_unknown = host.as_com_ref::<IHostApplication>().expect("HostApplication implements IHostApplication").as_ptr() as *mut FUnknown;
        // SAFETY: `host` lives in the Plugin, which keeps the module alive until it drops.
        unsafe { module.set_host_context(host_unknown) };

        let shared = Arc::new(Shared {
            pending: Mutex::new(Vec::with_capacity(MAX_PARAM_QUEUES)),
            reported: Mutex::new(Vec::new()),
            restart: AtomicI32::new(0),
            dirty: AtomicBool::new(false),
            process_lock: Mutex::new(()),
            reprocess: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            latency: AtomicU32::new(0),
            midi_map: Mutex::new(Arc::new(MidiMap::empty())),
            midi_map_rev: AtomicU32::new(0),
            layout: Mutex::new(Layout::default()),
            layout_rev: AtomicU32::new(0),
            listener: Mutex::new(None),
        });
        let sh = shared.clone();
        let handler = ComponentHandler::new(Arc::new(move |e| {
            match e {
                HandlerEvent::PerformEdit(id, v) => {
                    let mut p = sh.pending.lock();
                    if p.len() < MAX_PARAM_QUEUES * 4 {
                        p.push((id, v));
                    }
                    sh.dirty.store(true, Ordering::Relaxed);
                }
                HandlerEvent::Restart(flags) => {
                    sh.restart.fetch_or(flags, Ordering::AcqRel);
                }
                HandlerEvent::Dirty(d) => sh.dirty.store(d, Ordering::Relaxed),
                _ => {}
            }
            if let Some(l) = sh.listener.lock().clone() {
                l(e);
            }
        }));

        let cid_t = parse_cid(&class.cid)?;
        let component: ComPtr<IComponent> = module.create(&cid_t).with_context(|| format!("creating '{}'", class.name))?;
        // SAFETY: standard VST3 calls on objects we own; out-structs are zeroed POD.
        unsafe {
            check(component.initialize(host_unknown), "IComponent::initialize")?;
            let processor: ComPtr<IAudioProcessor> = match component.cast() {
                Some(p) => p,
                None => {
                    component.terminate();
                    bail!("'{}' has no IAudioProcessor", class.name);
                }
            };
            let mut controller_separate = false;
            let controller: Option<ComPtr<IEditController>> = match component.cast::<IEditController>() {
                Some(c) => Some(c),
                None => {
                    let mut ccid: TUID = [0; 16];
                    if component.getControllerClassId(&mut ccid) == kResultOk && ccid.iter().any(|&b| b != 0) {
                        match module.create::<IEditController>(&ccid) {
                            Ok(c) => {
                                if c.initialize(host_unknown) == kResultOk {
                                    controller_separate = true;
                                    Some(c)
                                } else {
                                    None
                                }
                            }
                            Err(_) => None,
                        }
                    } else {
                        None
                    }
                }
            };
            let mut plugin = Plugin {
                connection: None,
                controller,
                processor,
                component,
                handler,
                host,
                controller_separate,
                active: false,
                setup,
                class,
                inputs: vec![],
                outputs: vec![],
                event_inputs: 0,
                shared,
                processor_taken: false,
                module,
            };
            plugin.connect();
            if let Some(c) = &plugin.controller {
                let h = plugin.handler.as_com_ref::<IComponentHandler>().expect("handler").as_ptr();
                c.setComponentHandler(h);
            }
            plugin.sync_controller_from_component();
            plugin.configure_buses()?;
            plugin.rebuild_midi_map();
            plugin.activate()?;
            Ok(plugin)
        }
    }

    /// Connect component and controller so they can message each other (only when they're separate objects).
    fn connect(&mut self) {
        if !self.controller_separate {
            return;
        }
        let Some(ctrl) = &self.controller else { return };
        let (Some(a), Some(b)) = (self.component.cast::<IConnectionPoint>(), ctrl.cast::<IConnectionPoint>()) else { return };
        // SAFETY: both are live connection points owned by this plugin.
        unsafe {
            a.connect(b.as_ptr());
            b.connect(a.as_ptr());
        }
        self.connection = Some((a, b));
    }

    /// Hand the component's current state to the controller so its parameters match.
    fn sync_controller_from_component(&self) {
        let Some(ctrl) = &self.controller else { return };
        if !self.controller_separate {
            return;
        }
        let s = MemStream::empty();
        let st = s.as_com_ref::<IBStream>().expect("IBStream").as_ptr();
        // SAFETY: stream is a live IBStream we own.
        unsafe {
            if self.component.getState(st) == kResultOk {
                s.rewind();
                ctrl.setComponentState(st);
            }
        }
    }

    /// Main audio output stereo and active, main audio input active only for effects, first event input
    /// active; everything else inactive (it still gets buffers).
    fn configure_buses(&mut self) -> Result<()> {
        let c = &self.component;
        // SAFETY: bus queries with zeroed POD out-structs on a live component.
        unsafe {
            let n_in = c.getBusCount(kAudio as MediaType, kInput as BusDirection).max(0);
            let n_out = c.getBusCount(kAudio as MediaType, kOutput as BusDirection).max(0);
            let n_ev = c.getBusCount(kEvent as MediaType, kInput as BusDirection).max(0);
            if n_out == 0 {
                bail!("'{}' has no audio outputs", self.class.name);
            }
            let info = |dir: i32, i: i32| -> BusInfo {
                let mut b: BusInfo = std::mem::zeroed();
                c.getBusInfo(kAudio as MediaType, dir as BusDirection, i, &mut b);
                b
            };
            let instrument = self.class.is_instrument() || n_ev > 0 && n_in == 0;
            let mut ins: Vec<(BusInfo, SpeakerArrangement, bool)> = (0..n_in)
                .map(|i| {
                    let b = info(kInput as i32, i);
                    let main = b.busType == kMain as BusType;
                    let want = i == 0 && main && !instrument;
                    (b, if want { SpeakerArr::kStereo } else { arrangement_for(b.channelCount) }, want)
                })
                .collect();
            let mut outs: Vec<(BusInfo, SpeakerArrangement, bool)> = (0..n_out)
                .map(|i| {
                    let b = info(kOutput as i32, i);
                    let want = i == 0;
                    (b, if want { SpeakerArr::kStereo } else { arrangement_for(b.channelCount) }, want)
                })
                .collect();
            let mut in_arr: Vec<SpeakerArrangement> = ins.iter().map(|x| x.1).collect();
            let mut out_arr: Vec<SpeakerArrangement> = outs.iter().map(|x| x.1).collect();
            let ok = self.processor.setBusArrangements(in_arr.as_mut_ptr(), n_in, out_arr.as_mut_ptr(), n_out) == kResultTrue;
            // whatever was accepted (or not), use the arrangements the plugin reports now
            for (i, x) in ins.iter_mut().enumerate() {
                let mut a: SpeakerArrangement = 0;
                if self.processor.getBusArrangement(kInput as BusDirection, i as i32, &mut a) == kResultOk {
                    x.1 = a;
                } else if !ok {
                    x.1 = arrangement_for(x.0.channelCount);
                }
            }
            for (i, x) in outs.iter_mut().enumerate() {
                let mut a: SpeakerArrangement = 0;
                if self.processor.getBusArrangement(kOutput as BusDirection, i as i32, &mut a) == kResultOk {
                    x.1 = a;
                } else if !ok {
                    x.1 = arrangement_for(x.0.channelCount);
                }
            }
            for (i, x) in ins.iter().enumerate() {
                c.activateBus(kAudio as MediaType, kInput as BusDirection, i as i32, x.2 as TBool);
            }
            for (i, x) in outs.iter().enumerate() {
                c.activateBus(kAudio as MediaType, kOutput as BusDirection, i as i32, x.2 as TBool);
            }
            for i in 0..n_ev {
                c.activateBus(kEvent as MediaType, kInput as BusDirection, i, (i == 0) as TBool);
            }
            let bus = |x: &(BusInfo, SpeakerArrangement, bool)| Bus {
                name: str16(&x.0.name.map(|c| c as u16)),
                channels: x.1.count_ones() as usize,
                main: x.0.busType == kMain as BusType,
                active: x.2,
            };
            self.inputs = ins.iter().map(bus).collect();
            self.outputs = outs.iter().map(bus).collect();
            self.event_inputs = n_ev as usize;
            if self.outputs[0].channels == 0 {
                bail!("'{}' main output has no channels", self.class.name);
            }
            let layout = Layout {
                inputs: self.inputs.iter().map(|b| b.channels).collect(),
                outputs: self.outputs.iter().map(|b| b.channels).collect(),
                main_in: self.inputs.iter().position(|b| b.active),
            };
            let mut l = self.shared.layout.lock();
            if *l != layout {
                *l = layout;
                self.shared.layout_rev.fetch_add(1, Ordering::Release);
            }
            Ok(())
        }
    }

    fn activate(&mut self) -> Result<()> {
        // SAFETY: plain calls on live objects.
        unsafe {
            if self.processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32) != kResultOk {
                bail!("'{}' can't process 32-bit float audio", self.class.name);
            }
            let mut ps = ProcessSetup {
                processMode: if self.setup.offline { ProcessModes_::kOffline } else { ProcessModes_::kRealtime } as i32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: self.setup.max_block as i32,
                sampleRate: self.setup.sample_rate,
            };
            check(self.processor.setupProcessing(&mut ps), "IAudioProcessor::setupProcessing")?;
            check(self.component.setActive(1), "IComponent::setActive(true)")?;
            self.active = true;
            self.shared.latency.store(self.processor.getLatencySamples(), Ordering::Relaxed);
        }
        Ok(())
    }

    fn deactivate(&mut self) {
        if self.active {
            // SAFETY: plain calls on live objects; the caller holds the process lock or the processor is gone.
            unsafe {
                self.processor.setProcessing(0);
                self.component.setActive(0);
            }
            self.active = false;
            self.shared.reprocess.store(true, Ordering::Release);
        }
    }

    /// The audio-thread half. Can be taken once.
    pub fn processor(&mut self) -> Result<Processor> {
        if self.processor_taken {
            bail!("the processor was already taken");
        }
        self.processor_taken = true;
        Ok(Processor::new(self))
    }

    /// Receive handler events (parameter edits in the plugin UI, restart requests, dirty state).
    pub fn set_listener(&self, f: Option<HandlerSink>) {
        *self.shared.listener.lock() = f;
    }

    pub fn info(&self) -> PluginInfo {
        PluginInfo {
            class: self.class.clone(),
            factory: self.module.factory_info(),
            inputs: self.inputs.clone(),
            outputs: self.outputs.clone(),
            event_inputs: self.event_inputs,
            latency: self.latency(),
            // SAFETY: plain call.
            tail: unsafe { self.processor.getTailSamples() },
            // SAFETY: plain call.
            params: self.controller.as_ref().map_or(0, |c| unsafe { c.getParameterCount() }.max(0) as usize),
            single_component: !self.controller_separate,
            setup: self.setup,
        }
    }

    pub fn class(&self) -> &ClassInfo {
        &self.class
    }

    pub fn latency(&self) -> u32 {
        self.shared.latency.load(Ordering::Relaxed)
    }

    /// The plugin said its state changed since the last `state()` (edited in its UI, or `setDirty`).
    pub fn dirty(&self) -> bool {
        self.shared.dirty.load(Ordering::Relaxed)
    }

    pub fn controller(&self) -> Option<&ComPtr<IEditController>> {
        self.controller.as_ref()
    }

    // ---- parameters ------------------------------------------------------------------------------

    pub fn params(&self) -> Vec<ParamInfo> {
        let Some(c) = &self.controller else { return vec![] };
        // SAFETY: zeroed POD out-struct filled by the controller.
        unsafe {
            (0..c.getParameterCount().max(0))
                .filter_map(|i| {
                    let mut p: ParameterInfo = std::mem::zeroed();
                    (c.getParameterInfo(i, &mut p) == kResultOk).then(|| ParamInfo {
                        id: p.id,
                        title: str16(&p.title.map(|c| c as u16)),
                        short_title: str16(&p.shortTitle.map(|c| c as u16)),
                        units: str16(&p.units.map(|c| c as u16)),
                        step_count: p.stepCount,
                        default: p.defaultNormalizedValue,
                        unit_id: p.unitId,
                        flags: p.flags,
                    })
                })
                .collect()
        }
    }

    /// Current normalized value.
    pub fn param(&self, id: u32) -> Option<f64> {
        // SAFETY: plain call.
        self.controller.as_ref().map(|c| unsafe { c.getParamNormalized(id) })
    }

    /// How the plugin displays a normalized value ("-3.0 dB", "Sine").
    pub fn param_text(&self, id: u32, value: f64) -> Option<String> {
        let c = self.controller.as_ref()?;
        let mut s: String128 = [0; 128];
        // SAFETY: String128 out-buffer.
        (unsafe { c.getParamStringByValue(id, value, &mut s) } == kResultOk).then(|| str16(&s.map(|c| c as u16)))
    }

    /// The normalized value the plugin parses `text` as.
    pub fn param_from_text(&self, id: u32, text: &str) -> Option<f64> {
        let c = self.controller.as_ref()?;
        let mut w: Vec<TChar> = wide(text).into_iter().map(|c| c as TChar).collect();
        let mut v = 0.0;
        // SAFETY: NUL-terminated UTF-16 input, f64 out.
        (unsafe { c.getParamValueByString(id, w.as_mut_ptr(), &mut v) } == kResultOk && v.is_finite()).then_some(v.clamp(0.0, 1.0))
    }

    /// Plain (unit) value <-> normalized, as the controller defines it.
    pub fn param_to_plain(&self, id: u32, value: f64) -> Option<f64> {
        // SAFETY: plain call.
        self.controller.as_ref().map(|c| unsafe { c.normalizedParamToPlain(id, value) })
    }

    pub fn param_from_plain(&self, id: u32, plain: f64) -> Option<f64> {
        // SAFETY: plain call.
        self.controller.as_ref().map(|c| unsafe { c.plainParamToNormalized(id, plain) })
    }

    /// Set a parameter (normalized): the controller now, the processor at the start of its next block.
    pub fn set_param(&self, id: u32, value: f64) {
        let v = value.clamp(0.0, 1.0);
        if let Some(c) = &self.controller {
            // SAFETY: plain call.
            unsafe { c.setParamNormalized(id, v) };
        }
        self.shared.pending.lock().push((id, v));
    }

    /// Set only the controller's value (the processor keeps its own): for reading a display that only shows the
    /// current value. False without a controller or if it refuses.
    pub fn set_controller_param(&self, id: u32, value: f64) -> bool {
        // SAFETY: plain call.
        self.controller.as_ref().is_some_and(|c| unsafe { c.setParamNormalized(id, value.clamp(0.0, 1.0)) } == kResultOk)
    }

    /// Apply values the processor reported back to the controller (call from the UI thread now and then).
    pub fn sync_reported(&self) {
        let vals: Vec<_> = std::mem::take(&mut *self.shared.reported.lock());
        if let Some(c) = &self.controller {
            for (id, v) in vals {
                // SAFETY: plain call.
                unsafe { c.setParamNormalized(id, v) };
            }
        }
    }

    /// Parameter changes are waiting for the processor's next `process` call (until then the
    /// processor's state, and so `state()`, doesn't reflect them).
    pub fn has_pending_params(&self) -> bool {
        !self.shared.pending.lock().is_empty()
    }

    // ---- programs --------------------------------------------------------------------------------

    pub fn program_lists(&self) -> Vec<ProgramList> {
        let Some(units) = self.controller.as_ref().and_then(|c| c.cast::<IUnitInfo>()) else { return vec![] };
        let params = self.params();
        // SAFETY: zeroed POD out-structs filled by the plugin.
        unsafe {
            let unit_infos: Vec<UnitInfo> = (0..units.getUnitCount().max(0))
                .filter_map(|i| {
                    let mut u: UnitInfo = std::mem::zeroed();
                    (units.getUnitInfo(i, &mut u) == kResultOk).then_some(u)
                })
                .collect();
            (0..units.getProgramListCount().max(0))
                .filter_map(|i| {
                    let mut li: ProgramListInfo = std::mem::zeroed();
                    if units.getProgramListInfo(i, &mut li) != kResultOk {
                        return None;
                    }
                    let unit = unit_infos.iter().find(|u| u.programListId == li.id);
                    let unit_id = unit.map_or(kRootUnitId, |u| u.id);
                    let programs = (0..li.programCount.max(0))
                        .map(|k| {
                            let mut s: String128 = [0; 128];
                            units.getProgramName(li.id, k, &mut s);
                            str16(&s.map(|c| c as u16))
                        })
                        .collect();
                    let param = params.iter().find(|p| p.program_change() && p.unit_id == unit_id).map(|p| p.id);
                    Some(ProgramList {
                        id: li.id,
                        name: str16(&li.name.map(|c| c as u16)),
                        unit_id,
                        unit_name: unit.map(|u| str16(&u.name.map(|c| c as u16))).unwrap_or_default(),
                        programs,
                        param,
                    })
                })
                .collect()
        }
    }

    /// Select program `index` of a program list through its program-change parameter.
    pub fn set_program(&self, list: &ProgramList, index: usize) -> Result<()> {
        let id = list.param.ok_or_else(|| anyhow!("program list '{}' has no program-change parameter", list.name))?;
        if index >= list.programs.len() {
            bail!("program list '{}' has {} programs", list.name, list.programs.len());
        }
        let n = list.programs.len();
        let v = if n > 1 { index as f64 / (n - 1) as f64 } else { 0.0 };
        self.set_param(id, v);
        Ok(())
    }

    // ---- state -----------------------------------------------------------------------------------

    pub fn state(&self) -> Result<State> {
        let comp = MemStream::empty();
        // SAFETY: streams are live IBStreams we own.
        unsafe {
            check(self.component.getState(comp.as_com_ref::<IBStream>().expect("IBStream").as_ptr()), "IComponent::getState")?;
            let mut controller = vec![];
            if let Some(c) = &self.controller {
                let cs = MemStream::empty();
                if c.getState(cs.as_com_ref::<IBStream>().expect("IBStream").as_ptr()) == kResultOk {
                    controller = cs.bytes();
                }
            }
            self.shared.dirty.store(false, Ordering::Relaxed);
            Ok(State { component: comp.bytes(), controller })
        }
    }

    pub fn set_state(&self, s: &State) -> Result<()> {
        let comp = MemStream::new(s.component.clone());
        let cp = comp.as_com_ref::<IBStream>().expect("IBStream").as_ptr();
        // SAFETY: streams are live IBStreams we own.
        unsafe {
            check(self.component.setState(cp), "IComponent::setState")?;
            if let Some(c) = &self.controller {
                comp.rewind();
                let r = c.setComponentState(cp);
                if r != kResultOk && r != kNotImplemented {
                    log::warn(&format!("{}: setComponentState: {}", self.class.name, result_name(r)));
                }
                if !s.controller.is_empty() {
                    let cs = MemStream::new(s.controller.clone());
                    let r = c.setState(cs.as_com_ref::<IBStream>().expect("IBStream").as_ptr());
                    if r != kResultOk && r != kNotImplemented {
                        log::warn(&format!("{}: controller setState: {}", self.class.name, result_name(r)));
                    }
                }
            }
        }
        self.shared.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    // ---- restarts --------------------------------------------------------------------------------

    /// Handle restart requests (`restartComponent`) the plugin made since the last call. Returns the flags handled.
    /// Call from the UI thread.
    pub fn handle_restart(&mut self) -> i32 {
        let flags = self.shared.restart.swap(0, Ordering::AcqRel);
        if flags == 0 {
            return 0;
        }
        use RestartFlags_::*;
        if flags & (kReloadComponent | kLatencyChanged | kIoChanged) as i32 != 0 {
            let shared = self.shared.clone();
            let _g = shared.process_lock.lock();
            let saved = if flags & kReloadComponent as i32 != 0 { self.state().ok() } else { None };
            self.deactivate();
            if flags & kIoChanged as i32 != 0 {
                // channel counts may change; keep our arrangement choice and re-read what the plugin accepted
                if let Err(e) = self.configure_buses() {
                    log::warn(&format!("{}: reconfiguring buses: {e:#}", self.class.name));
                }
            }
            if let Some(s) = saved {
                let _ = self.set_state(&s);
            }
            if let Err(e) = self.activate() {
                log::warn(&format!("{}: reactivating: {e:#}", self.class.name));
            }
        }
        if flags & kMidiCCAssignmentChanged as i32 != 0 {
            self.rebuild_midi_map();
        }
        flags
    }

    /// Deactivate and reactivate: clears voices, delay lines and tails.
    pub fn reset(&mut self) -> Result<()> {
        let shared = self.shared.clone();
        let _g = shared.process_lock.lock();
        self.deactivate();
        self.activate()
    }

    /// Bus layout as configured (the processor uses it to size buffers).
    pub fn buses(&self) -> (&[Bus], &[Bus]) {
        (&self.inputs, &self.outputs)
    }

    fn rebuild_midi_map(&self) {
        let mut m = MidiMap::empty();
        if let Some(mm) = self.controller.as_ref().and_then(|c| c.cast::<IMidiMapping>()) {
            for ch in 0..16 {
                for ctrl in 0..CTRL_COUNT {
                    let mut id: ParamID = 0;
                    // SAFETY: plain query with an out-param.
                    if unsafe { mm.getMidiControllerAssignment(0, ch as i16, ctrl as CtrlNumber, &mut id) } == kResultOk {
                        m.map[ch * CTRL_COUNT + ctrl] = Some(id);
                    }
                }
            }
        }
        *self.shared.midi_map.lock() = Arc::new(m);
        self.shared.midi_map_rev.fetch_add(1, Ordering::Release);
    }

    pub fn midi_map(&self) -> Arc<MidiMap> {
        self.shared.midi_map.lock().clone()
    }

    /// Create the plugin's editor view (None if it has none).
    pub fn create_view(&self) -> Option<ComPtr<IPlugView>> {
        let c = self.controller.as_ref()?;
        // SAFETY: "editor" is the standard view name; the returned pointer is add-ref'd.
        unsafe { ComPtr::from_raw(c.createView(c"editor".as_ptr())) }
    }

    pub fn module(&self) -> &Arc<Module> {
        &self.module
    }

    pub fn host_context(&self) -> &ComWrapper<HostApplication> {
        &self.host
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let lock = self.shared.clone();
        let _g = lock.process_lock.lock();
        self.shared.terminated.store(true, Ordering::Release);
        self.deactivate();
        // SAFETY: teardown in the reverse order of setup, on live objects.
        unsafe {
            if let Some((a, b)) = self.connection.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = &self.controller {
                c.setComponentHandler(std::ptr::null_mut());
                if self.controller_separate {
                    c.terminate();
                }
            }
            self.controller = None;
            self.component.terminate();
        }
    }
}

fn arrangement_for(channels: i32) -> SpeakerArrangement {
    match channels {
        ..=0 => 0,
        1 => SpeakerArr::kMono,
        2 => SpeakerArr::kStereo,
        n => (1u64 << n.min(63)) - 1,
    }
}

/// The audio-thread half of a plugin: buffers, event and parameter lists, and `process`.
pub struct Processor {
    processor: ComPtr<IAudioProcessor>,
    events: ComWrapper<EventList>,
    out_events: ComWrapper<EventList>,
    in_params: ComWrapper<ParamChanges>,
    out_params: ComWrapper<ParamChanges>,
    ctx: ProcessContext,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    in_bus: Vec<AudioBusBuffers>,
    out_bus: Vec<AudioBusBuffers>,
    in_layout: Vec<usize>,
    out_layout: Vec<usize>,
    main_in: Option<usize>,
    setup: Setup,
    processing: bool,
    midi_map: Arc<MidiMap>,
    midi_map_rev: u32,
    layout_rev: u32,
    continuous: i64,
    started: std::time::Instant,
    shared: Arc<Shared>,
    _module: Arc<Module>,
}

// SAFETY: the raw pointers point into buffers this struct owns; COM objects are used from one thread at a time.
unsafe impl Send for Processor {}

impl Processor {
    /// See [`Plugin::has_pending_params`].
    pub fn has_pending_params(&self) -> bool {
        !self.shared.pending.lock().is_empty()
    }

    fn new(p: &Plugin) -> Processor {
        let mut me = Processor {
            processor: p.processor.clone(),
            events: EventList::new(MAX_EVENTS),
            out_events: EventList::new(MAX_EVENTS),
            in_params: ParamChanges::new(MAX_PARAM_QUEUES, MAX_POINTS),
            out_params: ParamChanges::new(MAX_PARAM_QUEUES, MAX_POINTS),
            // SAFETY: ProcessContext is POD.
            ctx: unsafe { std::mem::zeroed() },
            in_ptrs: vec![],
            out_ptrs: vec![],
            in_bufs: vec![],
            out_bufs: vec![],
            in_bus: vec![],
            out_bus: vec![],
            in_layout: vec![],
            out_layout: vec![],
            main_in: None,
            setup: p.setup,
            processing: false,
            midi_map: p.midi_map(),
            midi_map_rev: p.shared.midi_map_rev.load(Ordering::Acquire),
            layout_rev: u32::MAX,
            continuous: 0,
            started: std::time::Instant::now(),
            shared: p.shared.clone(),
            _module: p.module.clone(),
        };
        me.relayout();
        me
    }

    /// Size buffers for the plugin's current bus layout. Allocates: runs at creation and after the
    /// plugin changes its buses (kIoChanged), which is rare and already interrupts playback.
    fn relayout(&mut self) {
        let rev = self.shared.layout_rev.load(Ordering::Acquire);
        if rev == self.layout_rev {
            return;
        }
        let l = self.shared.layout.lock().clone();
        let mb = self.setup.max_block;
        self.in_bufs = (0..l.inputs.iter().sum()).map(|_| vec![0.0; mb]).collect();
        self.out_bufs = (0..l.outputs.iter().sum()).map(|_| vec![0.0; mb]).collect();
        self.in_layout = l.inputs;
        self.out_layout = l.outputs;
        self.main_in = l.main_in;
        self.layout_rev = rev;
        self.bind();
    }

    /// Point the bus structs at our buffers (after (re)allocation).
    fn bind(&mut self) {
        self.in_ptrs = self.in_bufs.iter_mut().map(|b| b.as_mut_ptr()).collect();
        self.out_ptrs = self.out_bufs.iter_mut().map(|b| b.as_mut_ptr()).collect();
        let mk = |layout: &[usize], ptrs: &mut Vec<*mut f32>| -> Vec<AudioBusBuffers> {
            let mut at = 0;
            layout
                .iter()
                .map(|&n| {
                    let p = if n == 0 { std::ptr::null_mut() } else { ptrs[at..].as_mut_ptr() };
                    at += n;
                    AudioBusBuffers { numChannels: n as i32, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: p } }
                })
                .collect()
        };
        self.in_bus = mk(&self.in_layout, &mut self.in_ptrs);
        self.out_bus = mk(&self.out_layout, &mut self.out_ptrs);
    }

    pub fn setup(&self) -> Setup {
        self.setup
    }

    pub fn latency(&self) -> u32 {
        self.shared.latency.load(Ordering::Relaxed)
    }

    /// Channels of the main output bus.
    pub fn out_channels(&self) -> usize {
        self.out_layout.first().copied().unwrap_or(0)
    }

    /// Run one block of up to any length (split internally into `max_block` pieces).
    ///
    /// `inputs`: the main input's channels (ignored for instruments); `outputs`: stereo (or more) to fill
    /// from the main output (a mono plugin is copied to every channel). `midi` and `params` must be sorted
    /// by offset. Realtime-safe: no allocation, no blocking locks.
    pub fn process(
        &mut self,
        frames: usize,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi: &[TimedMidi],
        params: &[ParamPoint],
        transport: Option<&Transport>,
    ) -> ProcessReport {
        let mut report = ProcessReport::default();
        let mut done = 0;
        let (mut mi, mut pi) = (0, 0);
        while done < frames {
            let n = (frames - done).min(self.setup.max_block);
            let end = (done + n) as u32;
            let m_end = mi + midi[mi..].iter().take_while(|e| e.offset < end).count();
            let p_end = pi + params[pi..].iter().take_while(|e| e.offset < end).count();
            let t = transport.map(|t| advance(t, done, self.setup.sample_rate));
            let r = self.block(done, n, inputs, outputs, &midi[mi..m_end], &params[pi..p_end], t.as_ref());
            report.skipped |= r.skipped;
            report.unmapped += r.unmapped;
            report.dropped += r.dropped;
            mi = m_end;
            pi = p_end;
            done += n;
        }
        report
    }

    #[allow(clippy::too_many_arguments)]
    fn block(
        &mut self,
        at: usize,
        n: usize,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        midi: &[TimedMidi],
        params: &[ParamPoint],
        transport: Option<&Transport>,
    ) -> ProcessReport {
        let mut report = ProcessReport::default();
        let shared = self.shared.clone();
        let Some(_g) = shared.process_lock.try_lock() else {
            silence(outputs, at, n);
            report.skipped = true;
            return report;
        };
        if shared.terminated.load(Ordering::Acquire) {
            silence(outputs, at, n);
            report.skipped = true;
            return report;
        }
        if shared.reprocess.swap(false, Ordering::AcqRel) {
            self.processing = false;
        }
        self.relayout();
        if self.out_layout.first().is_none_or(|&c| c == 0) {
            silence(outputs, at, n);
            report.skipped = true;
            return report;
        }
        if !self.processing {
            // SAFETY: plain call; some plugins return kNotImplemented, which is fine.
            unsafe { self.processor.setProcessing(1) };
            self.processing = true;
        }
        let rev = shared.midi_map_rev.load(Ordering::Acquire);
        if rev != self.midi_map_rev
            && let Some(m) = shared.midi_map.try_lock()
        {
            self.midi_map = m.clone();
            self.midi_map_rev = rev;
        }

        self.events.clear();
        self.out_events.clear();
        self.in_params.clear();
        self.out_params.clear();
        if let Some(mut p) = shared.pending.try_lock() {
            for (id, v) in p.drain(..) {
                if !self.in_params.add(id, 0, v) {
                    report.dropped += 1;
                }
            }
        }
        for e in midi {
            let off = (e.offset as usize).saturating_sub(at).min(n - 1) as i32;
            let ok = match e.msg {
                Midi::NoteOn { ch, pitch, vel } if vel > 0 => self.events.push(note_on(off, ch as i16, pitch as i16, vel as f32 / 127.0, -1)),
                Midi::NoteOn { ch, pitch, .. } => self.events.push(note_off(off, ch as i16, pitch as i16, 0.0, -1)),
                Midi::NoteOff { ch, pitch, vel } => self.events.push(note_off(off, ch as i16, pitch as i16, vel as f32 / 127.0, -1)),
                Midi::Cc { ch, cc, value } => match self.midi_map.get(ch, cc as usize) {
                    Some(id) => self.in_params.add(id, off, value as f64 / 127.0),
                    None => {
                        report.unmapped += 1;
                        true
                    }
                },
                Midi::ChannelPressure { ch, value } => match self.midi_map.get(ch, ControllerNumbers_::kAfterTouch as usize) {
                    Some(id) => self.in_params.add(id, off, value as f64 / 127.0),
                    None => {
                        report.unmapped += 1;
                        true
                    }
                },
                Midi::PitchBend { ch, value } => match self.midi_map.get(ch, ControllerNumbers_::kPitchBend as usize) {
                    Some(id) => self.in_params.add(id, off, (value.clamp(-8192, 8191) as f64 + 8192.0) / 16383.0),
                    None => {
                        report.unmapped += 1;
                        true
                    }
                },
            };
            if !ok {
                report.dropped += 1;
            }
        }
        for p in params {
            let off = (p.offset as usize).saturating_sub(at).min(n - 1) as i32;
            if !self.in_params.add(p.id, off, p.value.clamp(0.0, 1.0)) {
                report.dropped += 1;
            }
        }

        // inputs: the main input bus gets the caller's channels, everything else silence
        let mut ch0 = 0;
        for (b, &chs) in self.in_layout.iter().enumerate() {
            for c in 0..chs {
                let buf = &mut self.in_bufs[ch0 + c][..n];
                match (Some(b) == self.main_in, inputs.get(c).or(inputs.last())) {
                    (true, Some(src)) if src.len() >= at + n => buf.copy_from_slice(&src[at..at + n]),
                    _ => buf.fill(0.0),
                }
            }
            self.in_bus[b].silenceFlags = if Some(b) == self.main_in && !inputs.is_empty() { 0 } else { (1u64 << chs.min(63)) - 1 };
            ch0 += chs;
        }
        for b in &mut self.out_bufs {
            b[..n].fill(0.0);
        }
        for b in &mut self.out_bus {
            b.silenceFlags = 0;
        }

        let mut ctx_ptr = std::ptr::null_mut();
        if let Some(t) = transport {
            self.fill_context(t);
            ctx_ptr = &mut self.ctx as *mut ProcessContext;
        }
        let mut data = ProcessData {
            processMode: if self.setup.offline { ProcessModes_::kOffline } else { ProcessModes_::kRealtime } as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: n as i32,
            numInputs: self.in_bus.len() as i32,
            numOutputs: self.out_bus.len() as i32,
            inputs: if self.in_bus.is_empty() { std::ptr::null_mut() } else { self.in_bus.as_mut_ptr() },
            outputs: self.out_bus.as_mut_ptr(),
            inputParameterChanges: self.in_params.as_com_ref::<IParameterChanges>().expect("IParameterChanges").as_ptr(),
            outputParameterChanges: self.out_params.as_com_ref::<IParameterChanges>().expect("IParameterChanges").as_ptr(),
            inputEvents: self.events.as_com_ref::<IEventList>().expect("IEventList").as_ptr(),
            outputEvents: self.out_events.as_com_ref::<IEventList>().expect("IEventList").as_ptr(),
            processContext: ctx_ptr,
        };
        // SAFETY: every pointer in `data` refers to buffers/objects owned by self and sized for n <= max_block.
        let r = unsafe { self.processor.process(&mut data) };
        if r != kResultOk {
            silence(outputs, at, n);
            report.skipped = true;
            return report;
        }
        self.continuous += n as i64;

        // main output -> caller
        let main = self.out_layout[0];
        for (c, out) in outputs.iter_mut().enumerate() {
            if out.len() < at + n {
                continue;
            }
            let src = &self.out_bufs[c.min(main - 1)][..n];
            let dst = &mut out[at..at + n];
            for (d, s) in dst.iter_mut().zip(src) {
                *d = if s.is_finite() { *s } else { 0.0 };
            }
        }
        // values the plugin changed itself, for the controller
        if !self.out_params.is_empty()
            && let Some(mut rep) = shared.reported.try_lock()
        {
            for i in 0..self.out_params.len() {
                if let Some(q) = self.out_params.queue(i)
                    && let Some(&(_, v)) = q.points().last()
                    && rep.len() < MAX_PARAM_QUEUES * 4
                {
                    rep.push((q.id(), v));
                }
            }
        }
        report
    }

    fn fill_context(&mut self, t: &Transport) {
        use ProcessContext_::StatesAndFlags_::*;
        let mut state = kTempoValid | kTimeSigValid | kProjectTimeMusicValid | kBarPositionValid | kSystemTimeValid | kContTimeValid;
        if t.playing {
            state |= kPlaying;
        }
        if let Some((a, b)) = t.cycle {
            state |= kCycleActive | kCycleValid;
            self.ctx.cycleStartMusic = a;
            self.ctx.cycleEndMusic = b;
        }
        self.ctx.state = state as u32;
        self.ctx.sampleRate = self.setup.sample_rate;
        self.ctx.projectTimeSamples = t.project_samples;
        self.ctx.systemTime = self.started.elapsed().as_nanos() as i64;
        self.ctx.continousTimeSamples = self.continuous;
        self.ctx.projectTimeMusic = t.ppq;
        self.ctx.barPositionMusic = t.bar_ppq;
        self.ctx.tempo = t.tempo;
        self.ctx.timeSigNumerator = t.ts_num;
        self.ctx.timeSigDenominator = t.ts_den;
    }

    /// Call `setProcessing(false)` (e.g. when playback stops for a while); the next block restarts it.
    pub fn stop(&mut self) {
        let shared = self.shared.clone();
        let _g = shared.process_lock.lock();
        if self.processing && !shared.terminated.load(Ordering::Acquire) {
            // SAFETY: plain call.
            unsafe { self.processor.setProcessing(0) };
        }
        self.processing = false;
    }
}

impl Drop for Processor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Transport moved forward by `frames` (for sub-blocks).
pub(crate) fn advance(t: &Transport, frames: usize, sr: f64) -> Transport {
    if frames == 0 {
        return *t;
    }
    let mut o = *t;
    o.project_samples += frames as i64;
    let dq = frames as f64 / sr * t.tempo / 60.0;
    o.ppq += dq;
    let bar_q = t.ts_num as f64 * 4.0 / t.ts_den.max(1) as f64;
    if o.ppq - o.bar_ppq >= bar_q {
        o.bar_ppq += ((o.ppq - o.bar_ppq) / bar_q).floor() * bar_q;
    }
    o
}

fn silence(outputs: &mut [&mut [f32]], at: usize, n: usize) {
    for o in outputs.iter_mut() {
        if o.len() >= at + n {
            o[at..at + n].fill(0.0);
        }
    }
}

/// Minimal logging to stderr (the plugin host's stderr is captured by the client).
pub mod log {
    pub fn warn(msg: &str) {
        eprintln!("[flux-vst3] warning: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_advances_bars() {
        let t = Transport { playing: true, tempo: 120.0, ts_num: 4, ts_den: 4, project_samples: 0, ppq: 3.5, bar_ppq: 0.0, cycle: None };
        // 0.5 s at 120 bpm = one quarter note
        let o = advance(&t, 24000, 48000.0);
        assert!((o.ppq - 4.5).abs() < 1e-9);
        assert_eq!(o.bar_ppq, 4.0);
        assert_eq!(o.project_samples, 24000);
    }

    #[test]
    fn arrangements() {
        assert_eq!(arrangement_for(2), SpeakerArr::kStereo);
        assert_eq!(arrangement_for(1), SpeakerArr::kMono);
        assert_eq!(arrangement_for(6).count_ones(), 6);
        assert_eq!(arrangement_for(0), 0);
    }
}
