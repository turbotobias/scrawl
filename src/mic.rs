//! Which microphone Mac captures record from: `"microphone": {"id", "name"}` in
//! `~/.config/scrawl/config.json` (absent = the macOS default, followed as it changes). The id is
//! the CoreAudio device UID, which is also `AVCaptureDevice.uniqueID`; the name is kept only to
//! say which device is missing. A chosen device that isn't connected falls back to the default.
//!
//! With the `backupMics` experiment a capture records from several inputs at once
//! (`recording_order`): `"microphones": [{"id", "name"}, …]` in priority order, where the id
//! `"default"` is whichever input macOS has as its default then. Without that key it's the chosen
//! input (or the default) and then the built-in microphone.

use serde::{Deserialize, Serialize};

/// The `microphone` key: a device the user picked in Settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub id: String,
    pub name: String,
}

/// The input devices connected now, in CoreAudio's order, and the default's id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inputs {
    pub devices: Vec<Input>,
    pub default: Option<String>,
    /// The Mac's own microphone, if it has one (a MacBook's or iMac's, not a Mac Studio's).
    pub built_in: Option<String>,
}

impl Inputs {
    pub fn find(&self, id: &str) -> Option<&Input> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn default_input(&self) -> Option<&Input> {
        self.default.as_deref().and_then(|id| self.find(id))
    }
}

/// What a recording uses, given the choice and what's connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick<'a> {
    /// No choice: the macOS default (none when there's no microphone at all).
    Default(Option<&'a Input>),
    Chosen(&'a Input),
    /// The chosen device isn't connected; the default stands in.
    Missing {
        wanted: &'a Choice,
        using: Option<&'a Input>,
    },
}

impl<'a> Pick<'a> {
    pub fn new(choice: Option<&'a Choice>, inputs: &'a Inputs) -> Self {
        match choice {
            None => Pick::Default(inputs.default_input()),
            Some(wanted) => match inputs.find(&wanted.id) {
                Some(input) => Pick::Chosen(input),
                None => Pick::Missing {
                    wanted,
                    using: inputs.default_input(),
                },
            },
        }
    }

    /// The device that will record.
    pub fn device(&self) -> Option<&'a Input> {
        match *self {
            Pick::Default(input) | Pick::Missing { using: input, .. } => input,
            Pick::Chosen(input) => Some(input),
        }
    }

    /// "AirPods Pro isn't connected. Using MacBook Pro Microphone." for a missing device.
    pub fn note(&self) -> Option<String> {
        let Pick::Missing { wanted, using } = self else {
            return None;
        };
        Some(match using {
            Some(using) => format!("{} isn't connected. Using {}.", wanted.name, using.name),
            None => format!(
                "{} isn't connected, and there's no other microphone.",
                wanted.name
            ),
        })
    }
}

/// The configured choice, if any.
pub fn choice() -> Option<Choice> {
    choice_in(&crate::handoff::default_config_path())
}

pub fn choice_in(path: &std::path::Path) -> Option<Choice> {
    crate::handoff::load_config_from(path)
        .microphone
        .filter(|c| !c.id.trim().is_empty())
}

/// Saves a pick from Settings: `"default"` clears the key; an id that isn't connected (so it
/// can't be named) changes nothing.
pub fn save_in(path: &std::path::Path, id: &str, inputs: &Inputs) -> anyhow::Result<()> {
    let value = if id == "default" {
        serde_json::Value::Null
    } else if let Some(input) = inputs.find(id) {
        serde_json::to_value(Choice {
            id: input.id.clone(),
            name: input.name.clone(),
        })?
    } else {
        return Ok(());
    };
    crate::handoff::set_config_value_in(path, "microphone", value)
}

/// Enumerates input devices (CoreAudio, a few ms; never on the UI thread).
pub fn inputs() -> Inputs {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let devices: Vec<Input> = host
        .input_devices()
        .map(|devices| {
            devices
                .filter_map(|device| {
                    let id = device.id().ok()?.id().to_owned();
                    let name = device.description().ok()?.name().to_owned();
                    Some(Input { id, name })
                })
                .collect()
        })
        .unwrap_or_default();
    let default = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.id().to_owned());
    #[cfg(target_os = "macos")]
    let built_in = transport::built_in()
        .into_iter()
        .find(|id| devices.iter().any(|d| &d.id == id));
    #[cfg(not(target_os = "macos"))]
    let built_in = None;
    Inputs {
        devices,
        default,
        built_in,
    }
}

/// The `microphones` entry for the macOS default input.
pub const SYSTEM_DEFAULT: &str = "default";
/// The most inputs a capture records from at once.
pub const MOST_AT_ONCE: usize = 3;

/// The saved order (`microphones`), if any.
pub fn order() -> Option<Vec<Choice>> {
    order_in(&crate::handoff::default_config_path())
}

pub fn order_in(path: &std::path::Path) -> Option<Vec<Choice>> {
    crate::handoff::load_config_from(path)
        .microphones
        .map(|order| {
            order
                .into_iter()
                .filter(|c| !c.id.trim().is_empty())
                .collect()
        })
}

/// The order Settings shows and a capture follows: the saved one, else the chosen input (or the
/// macOS default) and then the built-in microphone.
pub fn effective_order(
    saved: Option<&[Choice]>,
    choice: Option<&Choice>,
    inputs: &Inputs,
) -> Vec<Choice> {
    if let Some(saved) = saved {
        return saved.to_vec();
    }
    let first = choice.cloned().unwrap_or_else(|| Choice {
        id: SYSTEM_DEFAULT.into(),
        name: "System default".into(),
    });
    let built_in = inputs
        .built_in
        .as_deref()
        .and_then(|id| inputs.find(id))
        .filter(|input| input.id != first.id)
        .map(|input| Choice {
            id: input.id.clone(),
            name: input.name.clone(),
        });
    std::iter::once(first).chain(built_in).collect()
}

/// What a capture records from (`backupMics`), first the one it prefers: `effective_order`'s
/// inputs that are connected, each once (the default counts as the device it is), at most
/// `MOST_AT_ONCE`. A chosen input that isn't connected gives way to the default, as `Pick` does.
pub fn recording_order(
    saved: Option<&[Choice]>,
    choice: Option<&Choice>,
    inputs: &Inputs,
) -> Vec<Input> {
    let mut order: Vec<Input> = Vec::new();
    let mut wanted: Vec<&str> = Vec::new();
    let effective = effective_order(saved, choice, inputs);
    for entry in &effective {
        wanted.push(&entry.id);
    }
    if saved.is_none() && choice.is_some_and(|c| inputs.find(&c.id).is_none()) {
        wanted[0] = SYSTEM_DEFAULT;
    }
    for id in wanted {
        let input = if id == SYSTEM_DEFAULT {
            inputs.default_input()
        } else {
            inputs.find(id)
        };
        if let Some(input) = input
            && !order.iter().any(|o| o.id == input.id)
        {
            order.push(input.clone());
        }
    }
    order.truncate(MOST_AT_ONCE);
    order
}

/// Saves the order from Settings (`ids`, first preferred): each named from what's connected, or as
/// it was saved before when it isn't, and the default as "System default". Ids it can't name are
/// left out.
pub fn save_order_in(
    path: &std::path::Path,
    ids: &[String],
    inputs: &Inputs,
    before: &[Choice],
) -> anyhow::Result<()> {
    let mut order: Vec<Choice> = Vec::new();
    for id in ids {
        let name = if id == SYSTEM_DEFAULT {
            Some("System default".to_owned())
        } else {
            inputs
                .find(id)
                .map(|input| input.name.clone())
                .or_else(|| before.iter().find(|c| &c.id == id).map(|c| c.name.clone()))
        };
        if let Some(name) = name
            && !order.iter().any(|c| &c.id == id)
        {
            order.push(Choice {
                id: id.clone(),
                name,
            });
        }
    }
    crate::handoff::set_config_value_in(path, "microphones", serde_json::to_value(order)?)
}

/// The cpal device for `id`, if connected.
pub fn device(id: &str) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    cpal::default_host()
        .input_devices()
        .ok()?
        .find(|device| device.id().is_ok_and(|d| d.id() == id))
}

/// Logs a missing chosen device once, until it's back or another goes missing.
pub fn note_pick(pick: &Pick) {
    static WARNED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match (pick, pick.note()) {
        (Pick::Missing { wanted, .. }, Some(note)) => {
            if warned.as_deref() != Some(wanted.id.as_str()) {
                crate::capture::scrawl_log(format!("Microphone: {note}"));
                *warned = Some(wanted.id.clone());
            }
        }
        _ => *warned = None,
    }
}

/// Calls `changed` on its own thread once at the start, then whenever an input device is plugged
/// in or out or the macOS default changes (bursts coalesced). Stops when dropped.
pub struct Watch {
    #[cfg(target_os = "macos")]
    sender: std::sync::mpsc::Sender<watch::Message>,
}

impl Watch {
    pub fn start(changed: impl Fn() + Send + 'static) -> Self {
        #[cfg(target_os = "macos")]
        {
            Self {
                sender: watch::start(Box::new(changed)),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::thread::spawn(changed);
            Self {}
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        let _ = self.sender.send(watch::Message::Stop);
    }
}

#[cfg(target_os = "macos")]
mod watch {
    use std::ffi::c_void;
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
    use std::time::Duration;

    pub enum Message {
        Changed,
        Stop,
    }

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    type Listener = unsafe extern "C" fn(u32, u32, *const Address, *mut c_void) -> i32;

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectAddPropertyListener(
            object: u32,
            address: *const Address,
            listener: Listener,
            data: *mut c_void,
        ) -> i32;
        fn AudioObjectRemovePropertyListener(
            object: u32,
            address: *const Address,
            listener: Listener,
            data: *mut c_void,
        ) -> i32;
    }

    const SYSTEM_OBJECT: u32 = 1;
    const SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
    const DEVICES: u32 = u32::from_be_bytes(*b"dev#");
    const DEFAULT_INPUT: u32 = u32::from_be_bytes(*b"dIn ");

    unsafe extern "C" fn notify(_: u32, _: u32, _: *const Address, data: *mut c_void) -> i32 {
        // SAFETY: `data` is the boxed sender, freed only after both listeners are removed.
        let sender = unsafe { &*(data as *const Sender<Message>) };
        let _ = sender.send(Message::Changed);
        0
    }

    /// CoreAudio wants listeners added and removed on the same thread, so one thread owns them.
    pub fn start(changed: Box<dyn Fn() + Send>) -> Sender<Message> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let data = Box::into_raw(Box::new(sender.clone())) as *mut c_void as usize;
        let spawned = std::thread::Builder::new()
            .name("scrawl-mic-watch".into())
            .spawn(move || {
                let data = data as *mut c_void;
                let addresses = [DEVICES, DEFAULT_INPUT].map(|selector| Address {
                    selector,
                    scope: SCOPE_GLOBAL,
                    element: 0,
                });
                for address in &addresses {
                    // SAFETY: a valid address and a listener that only reads `data`.
                    unsafe { AudioObjectAddPropertyListener(SYSTEM_OBJECT, address, notify, data) };
                }
                changed();
                run(&receiver, &*changed);
                for address in &addresses {
                    // SAFETY: the same triple that was added.
                    unsafe {
                        AudioObjectRemovePropertyListener(SYSTEM_OBJECT, address, notify, data)
                    };
                }
                // SAFETY: both listeners are gone, so nothing reads `data` any more.
                drop(unsafe { Box::from_raw(data as *mut Sender<Message>) });
            });
        if let Err(error) = spawned {
            crate::capture::scrawl_log(format!("Microphone watch failed: {error}"));
        }
        sender
    }

    fn run(receiver: &Receiver<Message>, changed: &dyn Fn()) {
        while let Ok(Message::Changed) = receiver.recv() {
            // Plugging one device in fires several notifications; settle before reading.
            loop {
                match receiver.recv_timeout(Duration::from_millis(150)) {
                    Ok(Message::Changed) => continue,
                    Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => break,
                }
            }
            changed();
        }
    }
}

/// Which inputs are the Mac's own, by CoreAudio transport type (cpal doesn't say).
#[cfg(target_os = "macos")]
mod transport {
    use std::ffi::c_void;

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }

    const SYSTEM_OBJECT: u32 = 1;
    const SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
    const DEVICES: u32 = u32::from_be_bytes(*b"dev#");
    const TRANSPORT: u32 = u32::from_be_bytes(*b"tran");
    const UID: u32 = u32::from_be_bytes(*b"uid ");
    const BUILT_IN: u32 = u32::from_be_bytes(*b"bltn");

    fn address(selector: u32) -> Address {
        Address {
            selector,
            scope: SCOPE_GLOBAL,
            element: 0,
        }
    }

    /// The UIDs of every built-in audio device (its speakers too).
    pub fn built_in() -> Vec<String> {
        use core_foundation::base::TCFType;
        use core_foundation::string::{CFString, CFStringRef};
        let mut size = 0_u32;
        // SAFETY: a valid address and out pointer.
        if unsafe {
            AudioObjectGetPropertyDataSize(
                SYSTEM_OBJECT,
                &address(DEVICES),
                0,
                std::ptr::null(),
                &mut size,
            )
        } != 0
        {
            return Vec::new();
        }
        let mut ids = vec![0_u32; size as usize / 4];
        // SAFETY: `ids` holds `size` bytes.
        if unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &address(DEVICES),
                0,
                std::ptr::null(),
                &mut size,
                ids.as_mut_ptr().cast(),
            )
        } != 0
        {
            return Vec::new();
        }
        ids.truncate(size as usize / 4);
        ids.into_iter()
            .filter(|&id| {
                let mut transport = 0_u32;
                let mut size = 4_u32;
                // SAFETY: a u32 out value of 4 bytes.
                let status = unsafe {
                    AudioObjectGetPropertyData(
                        id,
                        &address(TRANSPORT),
                        0,
                        std::ptr::null(),
                        &mut size,
                        (&raw mut transport).cast(),
                    )
                };
                status == 0 && transport == BUILT_IN
            })
            .filter_map(|id| {
                let mut uid: CFStringRef = std::ptr::null();
                let mut size = std::mem::size_of::<CFStringRef>() as u32;
                // SAFETY: a CFStringRef out value, returned under the create rule.
                let status = unsafe {
                    AudioObjectGetPropertyData(
                        id,
                        &address(UID),
                        0,
                        std::ptr::null(),
                        &mut size,
                        (&raw mut uid).cast(),
                    )
                };
                (status == 0 && !uid.is_null())
                    .then(|| unsafe { CFString::wrap_under_create_rule(uid) }.to_string())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(id: &str, name: &str) -> Input {
        Input {
            id: id.into(),
            name: name.into(),
        }
    }

    fn inputs() -> Inputs {
        Inputs {
            devices: vec![
                input("BuiltInMicrophoneDevice", "MacBook Pro Microphone"),
                input("usb-1", "Shure MV7"),
            ],
            default: Some("BuiltInMicrophoneDevice".into()),
            built_in: Some("BuiltInMicrophoneDevice".into()),
        }
    }

    fn ids(order: &[Input]) -> Vec<&str> {
        order.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn backups_record_in_the_saved_order_or_else_the_choice_then_the_built_in_mic() {
        let mut inputs = inputs();
        let shure = Choice {
            id: "usb-1".into(),
            name: "Shure MV7".into(),
        };
        // Nothing saved: the default, which is the built-in mic, once.
        assert_eq!(
            ids(&recording_order(None, None, &inputs)),
            ["BuiltInMicrophoneDevice"]
        );
        // A choice, then the built-in mic as its backup.
        assert_eq!(
            ids(&recording_order(None, Some(&shure), &inputs)),
            ["usb-1", "BuiltInMicrophoneDevice"]
        );
        // AirPods chosen but gone (taken by the iPhone): the default stands in.
        assert_eq!(
            ids(&recording_order(None, Some(&airpods()), &inputs)),
            ["BuiltInMicrophoneDevice"]
        );
        // AirPods back and the default: AirPods, then the built-in mic.
        inputs.devices.push(input("airpods", "AirPods Pro"));
        inputs.default = Some("airpods".into());
        assert_eq!(
            ids(&recording_order(None, None, &inputs)),
            ["airpods", "BuiltInMicrophoneDevice"]
        );
        // A saved order: as saved, only what's connected, the default as the device it is.
        let saved = [
            Choice {
                id: "gone".into(),
                name: "Old mic".into(),
            },
            Choice {
                id: SYSTEM_DEFAULT.into(),
                name: "System default".into(),
            },
            shure.clone(),
            Choice {
                id: "airpods".into(),
                name: "AirPods Pro".into(),
            },
        ];
        assert_eq!(
            ids(&recording_order(Some(&saved), None, &inputs)),
            ["airpods", "usb-1"]
        );
        // No built-in mic (a Mac Studio): the choice alone.
        inputs.built_in = None;
        assert_eq!(
            ids(&recording_order(None, Some(&shure), &inputs)),
            ["usb-1"]
        );
    }

    #[test]
    fn the_order_saves_with_names_even_for_inputs_not_connected() {
        let dir = std::env::temp_dir().join(format!(
            "scrawl-mic-order-{}-{}",
            std::process::id(),
            crate::now_epoch_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let before = [airpods()];
        let ids = ["airpods", "default", "usb-1", "nobody", "usb-1"].map(String::from);
        save_order_in(&path, &ids, &inputs(), &before).unwrap();
        let names: Vec<_> = order_in(&path)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["AirPods Pro", "System default", "Shure MV7"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn airpods() -> Choice {
        Choice {
            id: "airpods".into(),
            name: "AirPods Pro".into(),
        }
    }

    #[test]
    fn a_missing_device_falls_back_to_the_default_and_says_so() {
        let inputs = inputs();
        let built_in = &inputs.devices[0];
        assert_eq!(Pick::new(None, &inputs), Pick::Default(Some(built_in)));
        assert_eq!(Pick::new(None, &inputs).note(), None);

        let shure = Choice {
            id: "usb-1".into(),
            name: "Old name".into(),
        };
        let pick = Pick::new(Some(&shure), &inputs);
        assert_eq!(pick, Pick::Chosen(&inputs.devices[1]));
        assert_eq!(pick.device().unwrap().name, "Shure MV7");

        let wanted = airpods();
        let pick = Pick::new(Some(&wanted), &inputs);
        assert_eq!(pick.device(), Some(built_in));
        assert_eq!(
            pick.note().unwrap(),
            "AirPods Pro isn't connected. Using MacBook Pro Microphone."
        );

        let none = Inputs::default();
        assert_eq!(Pick::new(None, &none).device(), None);
        assert_eq!(
            Pick::new(Some(&wanted), &none).note().unwrap(),
            "AirPods Pro isn't connected, and there's no other microphone."
        );
    }

    #[test]
    fn the_choice_round_trips_through_the_config_keeping_other_keys() {
        let dir = std::env::temp_dir().join(format!(
            "scrawl-mic-{}-{}",
            std::process::id(),
            crate::now_epoch_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"ingestToken":"secret"}"#).unwrap();
        let inputs = inputs();
        assert_eq!(choice_in(&path), None);

        save_in(&path, "usb-1", &inputs).unwrap();
        assert_eq!(
            choice_in(&path),
            Some(Choice {
                id: "usb-1".into(),
                name: "Shure MV7".into()
            })
        );
        save_in(&path, "airpods", &inputs).unwrap();
        assert_eq!(
            choice_in(&path).unwrap().id,
            "usb-1",
            "can't name it, so no change"
        );
        save_in(&path, "default", &inputs).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(config.get("microphone"), None);
        assert_eq!(config["ingestToken"], "secret");
        assert_eq!(choice_in(&path), None);

        std::fs::write(&path, r#"{"microphone":{"id":"","name":"x"}}"#).unwrap();
        assert_eq!(choice_in(&path), None, "an empty id is the default");
        std::fs::write(&path, r#"{"microphone":42,"ingestToken":"kept"}"#).unwrap();
        assert_eq!(choice_in(&path), None);
        assert_eq!(
            crate::handoff::load_config_from(&path)
                .ingest_token
                .as_deref(),
            Some("kept"),
            "a bad microphone value never drops the rest"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
