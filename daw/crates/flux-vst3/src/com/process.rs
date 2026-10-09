//! The per-block objects handed to `IAudioProcessor::process`: the event list (notes) and the
//! parameter changes (automation, MIDI CC mapped to parameters, edits from the plugin's UI).
//!
//! Both are allocated once with fixed capacity and reused every block, so the audio thread never
//! allocates. They are only touched by the thread that calls `process` (and by the plugin inside
//! that call), which is why the interior mutability below is sound.

use std::cell::UnsafeCell;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper};

/// Notes (and other events) for one block, sorted by sample offset.
pub struct EventList {
    events: UnsafeCell<Vec<Event>>,
    cap: usize,
}

// SAFETY: see the module docs; a list is used by one thread at a time (the processing thread), and
// handing the owning plugin to another thread happens between blocks.
unsafe impl Sync for EventList {}
unsafe impl Send for EventList {}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl EventList {
    pub fn new(cap: usize) -> ComWrapper<EventList> {
        ComWrapper::new(EventList { events: UnsafeCell::new(Vec::with_capacity(cap)), cap })
    }

    #[allow(clippy::mut_from_ref)]
    fn v(&self) -> &mut Vec<Event> {
        // SAFETY: single-threaded use (module docs); no reference outlives the call that made it.
        unsafe { &mut *self.events.get() }
    }

    pub fn clear(&self) {
        self.v().clear();
    }

    /// Add an event; returns false when the block's list is full (the event is dropped).
    pub fn push(&self, e: Event) -> bool {
        let v = self.v();
        if v.len() >= self.cap {
            return false;
        }
        v.push(e);
        true
    }

    /// Stable sort by sample offset (events at the same offset keep their order: note-offs before note-ons
    /// if the caller pushed them that way).
    pub fn sort(&self) {
        self.v().sort_by_key(|e| e.sampleOffset);
    }

    pub fn len(&self) -> usize {
        self.v().len()
    }

    pub fn is_empty(&self) -> bool {
        self.v().is_empty()
    }

    pub fn get(&self, i: usize) -> Option<Event> {
        self.v().get(i).copied()
    }
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        self.v().len() as int32
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        if e.is_null() || index < 0 {
            return kInvalidArgument;
        }
        match self.v().get(index as usize) {
            Some(ev) => {
                // SAFETY: checked non-null.
                unsafe { *e = *ev };
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        if self.push(unsafe { *e }) { kResultOk } else { kOutOfMemory }
    }
}

/// One parameter's value points within a block.
pub struct ParamQueue {
    id: UnsafeCell<ParamID>,
    points: UnsafeCell<Vec<(i32, f64)>>,
    cap: usize,
}

// SAFETY: as for EventList.
unsafe impl Sync for ParamQueue {}
unsafe impl Send for ParamQueue {}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl ParamQueue {
    fn new(cap: usize) -> ComWrapper<ParamQueue> {
        ComWrapper::new(ParamQueue { id: UnsafeCell::new(0), points: UnsafeCell::new(Vec::with_capacity(cap)), cap })
    }

    #[allow(clippy::mut_from_ref)]
    fn p(&self) -> &mut Vec<(i32, f64)> {
        // SAFETY: single-threaded use (module docs).
        unsafe { &mut *self.points.get() }
    }

    fn reset(&self, id: ParamID) {
        // SAFETY: single-threaded use.
        unsafe { *self.id.get() = id };
        self.p().clear();
    }

    pub fn id(&self) -> ParamID {
        // SAFETY: single-threaded use.
        unsafe { *self.id.get() }
    }

    pub fn points(&self) -> &[(i32, f64)] {
        self.p()
    }

    /// Insert keeping offsets sorted; a point at an existing offset replaces its value (as the SDK does).
    /// Returns the point's index, or None if the queue is full.
    pub fn add(&self, offset: i32, value: f64) -> Option<usize> {
        let p = self.p();
        match p.binary_search_by_key(&offset, |x| x.0) {
            Ok(i) => {
                p[i].1 = value;
                Some(i)
            }
            Err(i) => {
                if p.len() >= self.cap {
                    // full: keep the newest value for the last point rather than dropping the change
                    let last = p.len() - 1;
                    if i > last {
                        p[last] = (offset, value);
                        return Some(last);
                    }
                    return None;
                }
                p.insert(i, (offset, value));
                Some(i)
            }
        }
    }
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.p().len() as int32
    }

    unsafe fn getPoint(&self, index: int32, sample_offset: *mut int32, value: *mut ParamValue) -> tresult {
        if sample_offset.is_null() || value.is_null() || index < 0 {
            return kInvalidArgument;
        }
        match self.p().get(index as usize) {
            Some(&(o, v)) => {
                // SAFETY: checked non-null.
                unsafe {
                    *sample_offset = o;
                    *value = v;
                }
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn addPoint(&self, sample_offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        match self.add(sample_offset, value) {
            Some(i) => {
                if !index.is_null() {
                    // SAFETY: checked non-null.
                    unsafe { *index = i as int32 };
                }
                kResultOk
            }
            None => kOutOfMemory,
        }
    }
}

/// The set of parameter queues for one block (input or output).
pub struct ParamChanges {
    pool: Vec<ComWrapper<ParamQueue>>,
    used: UnsafeCell<usize>,
}

// SAFETY: as for EventList.
unsafe impl Sync for ParamChanges {}
unsafe impl Send for ParamChanges {}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl ParamChanges {
    /// Room for `params` distinct parameters per block with `points` points each.
    pub fn new(params: usize, points: usize) -> ComWrapper<ParamChanges> {
        ComWrapper::new(ParamChanges { pool: (0..params).map(|_| ParamQueue::new(points)).collect(), used: UnsafeCell::new(0) })
    }

    fn used(&self) -> usize {
        // SAFETY: single-threaded use.
        unsafe { *self.used.get() }
    }

    pub fn clear(&self) {
        // SAFETY: single-threaded use.
        unsafe { *self.used.get() = 0 };
    }

    pub fn len(&self) -> usize {
        self.used()
    }

    pub fn is_empty(&self) -> bool {
        self.used() == 0
    }

    pub fn queue(&self, i: usize) -> Option<&ParamQueue> {
        (i < self.used()).then(|| &*self.pool[i])
    }

    /// The queue for `id`, opening one if needed; None when every queue is in use.
    pub fn queue_for(&self, id: ParamID) -> Option<(usize, &ParamQueue)> {
        let n = self.used();
        if let Some(i) = self.pool[..n].iter().position(|q| q.id() == id) {
            return Some((i, &self.pool[i]));
        }
        if n >= self.pool.len() {
            return None;
        }
        self.pool[n].reset(id);
        // SAFETY: single-threaded use.
        unsafe { *self.used.get() = n + 1 };
        Some((n, &self.pool[n]))
    }

    /// Add one point; false if there was no room.
    pub fn add(&self, id: ParamID, offset: i32, value: f64) -> bool {
        self.queue_for(id).and_then(|(_, q)| q.add(offset, value)).is_some()
    }

    fn raw(&self, i: usize) -> *mut IParamValueQueue {
        self.pool[i].as_com_ref::<IParamValueQueue>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.used() {
            return std::ptr::null_mut();
        }
        // Not add-ref'd: the SDK's ParameterChanges hands out borrowed pointers too.
        self.raw(index as usize)
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: checked non-null.
        match self.queue_for(unsafe { *id }) {
            Some((i, _)) => {
                if !index.is_null() {
                    // SAFETY: checked non-null.
                    unsafe { *index = i as int32 };
                }
                self.raw(i)
            }
            None => std::ptr::null_mut(),
        }
    }
}

/// A note-on event.
pub fn note_on(offset: i32, channel: i16, pitch: i16, velocity: f32, note_id: i32) -> Event {
    Event {
        busIndex: 0,
        sampleOffset: offset,
        ppqPosition: 0.0,
        flags: 0,
        r#type: Event_::EventTypes_::kNoteOnEvent as u16,
        __field0: Event__type0 { noteOn: NoteOnEvent { channel, pitch, tuning: 0.0, velocity, length: 0, noteId: note_id } },
    }
}

/// A note-off event.
pub fn note_off(offset: i32, channel: i16, pitch: i16, velocity: f32, note_id: i32) -> Event {
    Event {
        busIndex: 0,
        sampleOffset: offset,
        ppqPosition: 0.0,
        flags: 0,
        r#type: Event_::EventTypes_::kNoteOffEvent as u16,
        __field0: Event__type0 { noteOff: NoteOffEvent { channel, pitch, velocity, noteId: note_id, tuning: 0.0 } },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::ComPtr;

    #[test]
    fn event_list_capacity_and_order() {
        let l = EventList::new(2);
        assert!(l.push(note_on(10, 0, 60, 1.0, -1)));
        assert!(l.push(note_off(5, 0, 60, 0.0, -1)));
        assert!(!l.push(note_on(1, 0, 62, 1.0, -1)));
        l.sort();
        let p: ComPtr<IEventList> = l.to_com_ptr().unwrap();
        unsafe {
            assert_eq!(p.getEventCount(), 2);
            let mut e = std::mem::zeroed::<Event>();
            assert_eq!(p.getEvent(0, &mut e), kResultOk);
            assert_eq!(e.sampleOffset, 5);
            assert_eq!(p.getEvent(2, &mut e), kResultFalse);
        }
        l.clear();
        assert!(l.is_empty());
    }

    #[test]
    fn param_changes_merge_and_sort() {
        let c = ParamChanges::new(2, 3);
        assert!(c.add(7, 100, 0.5));
        assert!(c.add(7, 0, 0.1));
        assert!(c.add(7, 100, 0.6));
        assert!(c.add(9, 0, 1.0));
        assert!(!c.add(11, 0, 1.0));
        assert_eq!(c.len(), 2);
        assert_eq!(c.queue(0).unwrap().points(), &[(0, 0.1), (100, 0.6)]);
        // full queue: a later point replaces the last one
        assert!(c.add(7, 200, 0.7));
        assert!(c.add(7, 300, 0.8));
        assert_eq!(c.queue(0).unwrap().points(), &[(0, 0.1), (100, 0.6), (300, 0.8)]);
        let p: ComPtr<IParameterChanges> = c.to_com_ptr().unwrap();
        unsafe {
            assert_eq!(p.getParameterCount(), 2);
            let q = p.getParameterData(1);
            assert!(!q.is_null());
            let q = vst3::ComRef::from_raw(q).unwrap();
            assert_eq!(q.getParameterId(), 9);
            let mut idx = 0;
            let q2 = p.addParameterData(&9, &mut idx);
            assert_eq!(idx, 1);
            assert_eq!(q2, q.as_ptr());
        }
        c.clear();
        assert!(c.is_empty());
    }
}
