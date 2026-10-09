//! The host-side objects a plugin talks back to: `IHostApplication` (the host context passed to
//! `initialize`), `IComponentHandler` (parameter edits and restart requests from the controller)
//! and `IPlugFrame` (editor resize requests).

use super::message::{AttributeList, Message};
use std::ffi::c_void;
use std::sync::Arc;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper, Interface};

pub const HOST_NAME: &str = "Flux";

/// The host context: name, object factory for messages, and which optional interfaces we support.
pub struct HostApplication;

impl Class for HostApplication {
    type Interfaces = (IHostApplication, IPlugInterfaceSupport);
}

impl HostApplication {
    pub fn new() -> ComWrapper<HostApplication> {
        ComWrapper::new(HostApplication)
    }
}

/// Interfaces a plugin may ask whether we use (`IPlugInterfaceSupport`).
fn supported(iid: &[u8; 16]) -> bool {
    [
        IComponent::IID,
        IAudioProcessor::IID,
        IEditController::IID,
        IConnectionPoint::IID,
        IUnitInfo::IID,
        IMidiMapping::IID,
        IProcessContextRequirements::IID,
        IPlugView::IID,
        IPlugViewContentScaleSupport::IID,
        IComponentHandler::IID,
        IComponentHandler2::IID,
        IPlugFrame::IID,
    ]
    .iter()
    .any(|i| i == iid)
}

/// # Safety
/// `p` must be null or point to a 16-byte id.
unsafe fn guid(p: *const TUID) -> Option<[u8; 16]> {
    if p.is_null() {
        return None;
    }
    // SAFETY: per the caller.
    let t = unsafe { &*p };
    Some(t.map(|b| b as u8))
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null; String128 is a fixed array.
        let buf = unsafe { &mut *name };
        let mut tmp = [0u16; 128];
        crate::util::write16(HOST_NAME, &mut tmp);
        for (d, s) in buf.iter_mut().zip(tmp) {
            *d = s as TChar;
        }
        kResultOk
    }

    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        if obj.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        unsafe { *obj = std::ptr::null_mut() };
        let (Some(cid), Some(iid)) = (unsafe { guid(cid) }, unsafe { guid(iid) }) else { return kInvalidArgument };
        let ptr = if cid == IMessage::IID && iid == IMessage::IID {
            Message::new().to_com_ptr::<IMessage>().map(|p| p.into_raw() as *mut c_void)
        } else if cid == IAttributeList::IID && iid == IAttributeList::IID {
            AttributeList::new().to_com_ptr::<IAttributeList>().map(|p| p.into_raw() as *mut c_void)
        } else {
            None
        };
        match ptr {
            Some(p) => {
                // SAFETY: checked non-null; ownership of one reference passes to the caller.
                unsafe { *obj = p };
                kResultOk
            }
            None => kResultFalse,
        }
    }
}

impl IPlugInterfaceSupportTrait for HostApplication {
    unsafe fn isPlugInterfaceSupported(&self, iid: *const TUID) -> tresult {
        match unsafe { guid(iid) } {
            Some(i) if supported(&i) => kResultTrue,
            _ => kResultFalse,
        }
    }
}

/// What the controller tells the host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HandlerEvent {
    BeginEdit(u32),
    /// A parameter changed in the plugin's own UI (normalized value). The host must forward it to the processor.
    PerformEdit(u32, f64),
    EndEdit(u32),
    /// `IComponentHandler::restartComponent` flags (RestartFlags).
    Restart(i32),
    /// The plugin's state differs from the last saved one (`IComponentHandler2::setDirty`).
    Dirty(bool),
    RequestOpenEditor,
}

pub type HandlerSink = Arc<dyn Fn(HandlerEvent) + Send + Sync>;

pub struct ComponentHandler {
    sink: HandlerSink,
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler, IComponentHandler2);
}

impl ComponentHandler {
    pub fn new(sink: HandlerSink) -> ComWrapper<ComponentHandler> {
        ComWrapper::new(ComponentHandler { sink })
    }
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        (self.sink)(HandlerEvent::BeginEdit(id));
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        (self.sink)(HandlerEvent::PerformEdit(id, value));
        kResultOk
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        (self.sink)(HandlerEvent::EndEdit(id));
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        (self.sink)(HandlerEvent::Restart(flags));
        kResultOk
    }
}

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, state: TBool) -> tresult {
        (self.sink)(HandlerEvent::Dirty(state != 0));
        kResultOk
    }

    unsafe fn requestOpenEditor(&self, _name: FIDString) -> tresult {
        (self.sink)(HandlerEvent::RequestOpenEditor);
        kResultOk
    }

    unsafe fn startGroupEdit(&self) -> tresult {
        kResultOk
    }

    unsafe fn finishGroupEdit(&self) -> tresult {
        kResultOk
    }
}

/// Size the plugin wants its editor to be (`IPlugFrame::resizeView`); returns whether the host resized.
pub type ResizeFn = Box<dyn Fn(*mut IPlugView, ViewRect) -> bool>;

pub struct PlugFrame {
    resize: ResizeFn,
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl PlugFrame {
    pub fn new(resize: ResizeFn) -> ComWrapper<PlugFrame> {
        ComWrapper::new(PlugFrame { resize })
    }
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        if new_size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        let r = unsafe { *new_size };
        if (self.resize)(view, r) { kResultOk } else { kResultFalse }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use vst3::ComPtr;

    #[test]
    fn host_creates_messages() {
        let h = HostApplication::new();
        let p: ComPtr<IHostApplication> = h.to_com_ptr().unwrap();
        unsafe {
            let mut cid = IMessage::IID.map(|b| b as std::ffi::c_char);
            let mut iid = cid;
            let mut obj = std::ptr::null_mut();
            assert_eq!(p.createInstance(&mut cid, &mut iid, &mut obj), kResultOk);
            let msg = ComPtr::<IMessage>::from_raw(obj as *mut IMessage).unwrap();
            msg.setMessageID(c"x".as_ptr());
            let mut cid = IComponent::IID.map(|b| b as std::ffi::c_char);
            let mut iid = cid;
            assert_eq!(p.createInstance(&mut cid, &mut iid, &mut obj), kResultFalse);
            assert!(obj.is_null());
            let mut name: String128 = [0; 128];
            p.getName(&mut name);
            assert_eq!(crate::util::str16(&name.map(|c| c as u16)), "Flux");
        }
    }

    #[test]
    fn handler_forwards_edits() {
        let got = Arc::new(Mutex::new(vec![]));
        let g = got.clone();
        let h = ComponentHandler::new(Arc::new(move |e| g.lock().push(e)));
        let p: ComPtr<IComponentHandler> = h.to_com_ptr().unwrap();
        unsafe {
            p.beginEdit(7);
            p.performEdit(7, 0.25);
            p.endEdit(7);
        }
        assert_eq!(*got.lock(), vec![HandlerEvent::BeginEdit(7), HandlerEvent::PerformEdit(7, 0.25), HandlerEvent::EndEdit(7)]);
    }
}
