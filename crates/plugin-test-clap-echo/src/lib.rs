//! Reference CLAP (CLever Audio Plug-in) Audio Effect Plugin.
//!
//! A minimal reference fixture implementing the standard CLAP C-ABI with:
//! - Plugin entry and factory exports (`clap_entry`)
//! - Audio processing with gain control
//! - Parameter automation extension (`clap.params`)
//! - State persistence extension (`clap.state`)

use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    clap_event_param_value, clap_input_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::params::{
    clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE,
};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::plugin_features::CLAP_PLUGIN_FEATURE_AUDIO_EFFECT;
use clap_sys::process::{clap_process, CLAP_PROCESS_CONTINUE};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

const PLUGIN_ID: &[u8] = b"org.audio_engine.clap_test_echo\0";
const PLUGIN_NAME: &[u8] = b"CLAP Test Echo\0";
const PLUGIN_VENDOR: &[u8] = b"Sudipta615\0";
const PLUGIN_URL: &[u8] = b"https://github.com/Sudipta615/audio_engine\0";
const PLUGIN_VERSION: &[u8] = b"1.0.0\0";
const PLUGIN_DESC: &[u8] = b"Reference CLAP echo and gain effect\0";

struct Features([*const c_char; 2]);
unsafe impl Sync for Features {}

static FEATURES: Features = Features([CLAP_PLUGIN_FEATURE_AUDIO_EFFECT.as_ptr(), std::ptr::null()]);

static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: PLUGIN_ID.as_ptr() as *const c_char,
    name: PLUGIN_NAME.as_ptr() as *const c_char,
    vendor: PLUGIN_VENDOR.as_ptr() as *const c_char,
    url: PLUGIN_URL.as_ptr() as *const c_char,
    manual_url: std::ptr::null(),
    support_url: std::ptr::null(),
    version: PLUGIN_VERSION.as_ptr() as *const c_char,
    description: PLUGIN_DESC.as_ptr() as *const c_char,
    features: FEATURES.0.as_ptr(),
};

struct EchoPluginData {
    gain: f64,
    _sample_rate: f64,
}

static PARAMS_EXT: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

static STATE_EXT: clap_plugin_state = clap_plugin_state {
    save: Some(state_save),
    load: Some(state_load),
};

unsafe extern "C" fn plugin_init(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn plugin_destroy(plugin: *const clap_plugin) {
    if !plugin.is_null() && !(*plugin).plugin_data.is_null() {
        let _ = Box::from_raw((*plugin).plugin_data as *mut EchoPluginData);
        let _ = Box::from_raw(plugin as *mut clap_plugin);
    }
}

unsafe extern "C" fn plugin_activate(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min_frames_count: u32,
    _max_frames_count: u32,
) -> bool {
    if plugin.is_null() || (*plugin).plugin_data.is_null() {
        return false;
    }
    let data = &mut *((*plugin).plugin_data as *mut EchoPluginData);
    data._sample_rate = sample_rate;
    true
}

unsafe extern "C" fn plugin_deactivate(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_reset(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_start_processing(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn plugin_stop_processing(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_process(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> i32 {
    if plugin.is_null() || (*plugin).plugin_data.is_null() || process.is_null() {
        return CLAP_PROCESS_CONTINUE;
    }
    let data = &mut *((*plugin).plugin_data as *mut EchoPluginData);
    let proc = &*process;

    // Drain incoming parameter changes
    if !proc.in_events.is_null() {
        let in_ev = &*proc.in_events;
        if let (Some(size_fn), Some(get_fn)) = (in_ev.size, in_ev.get) {
            let count = size_fn(proc.in_events);
            for i in 0..count {
                let header = get_fn(proc.in_events, i);
                if !header.is_null()
                    && (*header).space_id == CLAP_CORE_EVENT_SPACE_ID
                    && (*header).type_ == CLAP_EVENT_PARAM_VALUE
                {
                    let param_ev = &*(header as *const clap_event_param_value);
                    if param_ev.param_id == 0 {
                        data.gain = param_ev.value;
                    }
                }
            }
        }
    }

    // Process audio frames
    let frames = proc.frames_count as usize;
    if proc.audio_inputs_count > 0 && proc.audio_outputs_count > 0 {
        let in_buf = &*proc.audio_inputs;
        let out_buf = &mut *proc.audio_outputs;
        let channels = (in_buf.channel_count as usize).min(out_buf.channel_count as usize);

        for c in 0..channels {
            let src = *in_buf.data32.add(c);
            let dst = *out_buf.data32.add(c);
            let gain = data.gain as f32;
            for i in 0..frames {
                *dst.add(i) = *src.add(i) * gain;
            }
        }
    }

    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn plugin_get_extension(
    _plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    let id_str = CStr::from_ptr(id);
    if id_str == CLAP_EXT_PARAMS {
        &PARAMS_EXT as *const _ as *const c_void
    } else if id_str == CLAP_EXT_STATE {
        &STATE_EXT as *const _ as *const c_void
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn plugin_on_main_thread(_plugin: *const clap_plugin) {}

// Parameter extension implementations
unsafe extern "C" fn params_count(_plugin: *const clap_plugin) -> u32 {
    1
}

unsafe extern "C" fn params_get_info(
    _plugin: *const clap_plugin,
    param_index: u32,
    param_info: *mut clap_param_info,
) -> bool {
    if param_index != 0 || param_info.is_null() {
        return false;
    }
    let info = &mut *param_info;
    info.id = 0;
    info.flags = CLAP_PARAM_IS_AUTOMATABLE;
    info.cookie = std::ptr::null_mut();
    let name = b"Gain\0";
    let module = b"main\0";
    for (i, &b) in name.iter().enumerate() {
        info.name[i] = b as c_char;
    }
    for (i, &b) in module.iter().enumerate() {
        info.module[i] = b as c_char;
    }
    info.min_value = 0.0;
    info.max_value = 2.0;
    info.default_value = 1.0;
    true
}

unsafe extern "C" fn params_get_value(
    plugin: *const clap_plugin,
    param_id: u32,
    out_value: *mut f64,
) -> bool {
    if param_id != 0 || out_value.is_null() || plugin.is_null() || (*plugin).plugin_data.is_null() {
        return false;
    }
    let data = &*((*plugin).plugin_data as *const EchoPluginData);
    *out_value = data.gain;
    true
}

unsafe extern "C" fn params_value_to_text(
    _plugin: *const clap_plugin,
    _param_id: u32,
    value: f64,
    out_buffer: *mut c_char,
    out_buffer_capacity: u32,
) -> bool {
    if out_buffer.is_null() || out_buffer_capacity == 0 {
        return false;
    }
    let s = format!("{:.2}\0", value);
    let bytes = s.as_bytes();
    let len = bytes.len().min(out_buffer_capacity as usize);
    std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out_buffer, len);
    true
}

unsafe extern "C" fn params_text_to_value(
    _plugin: *const clap_plugin,
    _param_id: u32,
    param_value_text: *const c_char,
    out_value: *mut f64,
) -> bool {
    if param_value_text.is_null() || out_value.is_null() {
        return false;
    }
    let s = CStr::from_ptr(param_value_text).to_string_lossy();
    if let Ok(v) = s.parse::<f64>() {
        *out_value = v;
        true
    } else {
        false
    }
}

unsafe extern "C" fn params_flush(
    _plugin: *const clap_plugin,
    _in_events: *const clap_input_events,
    _out_events: *const clap_sys::events::clap_output_events,
) {
}

// State extension implementations
unsafe extern "C" fn state_save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    if plugin.is_null() || (*plugin).plugin_data.is_null() || stream.is_null() {
        return false;
    }
    let data = &*((*plugin).plugin_data as *const EchoPluginData);
    let ostream = &*stream;
    if let Some(write_fn) = ostream.write {
        let bytes = data.gain.to_le_bytes();
        let written = write_fn(stream, bytes.as_ptr() as *const c_void, bytes.len() as u64);
        written == bytes.len() as i64
    } else {
        false
    }
}

unsafe extern "C" fn state_load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    if plugin.is_null() || (*plugin).plugin_data.is_null() || stream.is_null() {
        return false;
    }
    let data = &mut *((*plugin).plugin_data as *mut EchoPluginData);
    let istream = &*stream;
    if let Some(read_fn) = istream.read {
        let mut bytes = [0u8; 8];
        let read = read_fn(stream, bytes.as_mut_ptr() as *mut c_void, 8);
        if read == 8 {
            data.gain = f64::from_le_bytes(bytes);
            true
        } else {
            false
        }
    } else {
        false
    }
}

// Factory implementations
static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_get_plugin_count),
    get_plugin_descriptor: Some(factory_get_plugin_descriptor),
    create_plugin: Some(factory_create_plugin),
};

unsafe extern "C" fn factory_get_plugin_count(_factory: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn factory_get_plugin_descriptor(
    _factory: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    if index == 0 {
        &DESCRIPTOR
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn factory_create_plugin(
    _factory: *const clap_plugin_factory,
    _host: *const clap_host,
    plugin_id: *const c_char,
) -> *const clap_plugin {
    if plugin_id.is_null() {
        return std::ptr::null();
    }
    let id_str = CStr::from_ptr(plugin_id);
    if id_str.to_bytes_with_nul() != PLUGIN_ID {
        return std::ptr::null();
    }

    let plugin_data = Box::new(EchoPluginData {
        gain: 1.0,
        _sample_rate: 48000.0,
    });

    let plugin = Box::new(clap_plugin {
        desc: &DESCRIPTOR,
        plugin_data: Box::into_raw(plugin_data) as *mut c_void,
        init: Some(plugin_init),
        destroy: Some(plugin_destroy),
        activate: Some(plugin_activate),
        deactivate: Some(plugin_deactivate),
        reset: Some(plugin_reset),
        start_processing: Some(plugin_start_processing),
        stop_processing: Some(plugin_stop_processing),
        process: Some(plugin_process),
        get_extension: Some(plugin_get_extension),
        on_main_thread: Some(plugin_on_main_thread),
    });

    Box::into_raw(plugin)
}

// Entry point implementation
unsafe extern "C" fn entry_init(_plugin_path: *const c_char) -> bool {
    INITIALIZED.store(true, Ordering::SeqCst);
    true
}

unsafe extern "C" fn entry_deinit() {
    INITIALIZED.store(false, Ordering::SeqCst);
}

unsafe extern "C" fn entry_get_factory(factory_id: *const c_char) -> *const c_void {
    if factory_id.is_null() {
        return std::ptr::null();
    }
    let id = CStr::from_ptr(factory_id);
    if id == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY as *const _ as *const c_void
    } else {
        std::ptr::null()
    }
}

#[no_mangle]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};
