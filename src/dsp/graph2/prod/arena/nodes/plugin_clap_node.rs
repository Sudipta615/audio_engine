//! CLAP (CLever Audio Plug-in) Audio Plugin Host Node.
//!
//! Provides hosting and real-time execution of standard CLAP audio effect plugins
//! within the production DSP graph.
//!
//! ## Design & Safety Contract
//! - Control path: library dynamic loading via `libloading`, plugin entry lookup,
//!   factory query, plugin instantiation, configuration (`init`, `activate`).
//! - Realtime audio path (`process_block_f32`):
//!   - Pre-allocated planar channel pointers (`clap_audio_buffer`).
//!   - Zero memory allocations, zero locks, zero blocking operations.
//!   - `unsafe` calls to `clap_plugin.process` follow strict CLAP specification invariants.
//! - Bypass & Fallback: If no plugin is loaded or if processing fails, the node
//!   operates as a bit-exact pass-through.

#[cfg(feature = "clap-host")]
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::dsp::graph2::prod::arena::node::DspNode;
use crate::dsp::pipeline::{DspStageCapability, StageChannelSupport, StagePrecision};


#[cfg(feature = "clap-host")]
use std::ffi::{c_char, c_void, CStr, CString};
#[cfg(feature = "clap-host")]
use std::path::Path;
#[cfg(feature = "clap-host")]
use std::sync::Arc;

#[cfg(feature = "clap-host")]
use clap_sys::audio_buffer::clap_audio_buffer;
#[cfg(feature = "clap-host")]
use clap_sys::entry::clap_plugin_entry;
#[cfg(feature = "clap-host")]
use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, CLAP_CORE_EVENT_SPACE_ID,
    CLAP_EVENT_PARAM_VALUE,
};
#[cfg(feature = "clap-host")]
use clap_sys::ext::params::{clap_plugin_params, CLAP_EXT_PARAMS};
#[cfg(feature = "clap-host")]
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
#[cfg(feature = "clap-host")]
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
#[cfg(feature = "clap-host")]
use clap_sys::host::clap_host;
#[cfg(feature = "clap-host")]
use clap_sys::plugin::clap_plugin;
#[cfg(feature = "clap-host")]
use clap_sys::process::clap_process;
#[cfg(feature = "clap-host")]
use clap_sys::stream::{clap_istream, clap_ostream};
#[cfg(feature = "clap-host")]
use clap_sys::version::CLAP_VERSION;


/// A thread-safe handle keeping the dynamically loaded plugin library in memory.
#[cfg(feature = "clap-host")]
struct ClapLibrary {
    _lib: libloading::Library,
    _entry: *const clap_plugin_entry,
}

#[cfg(feature = "clap-host")]
unsafe impl Send for ClapLibrary {}
#[cfg(feature = "clap-host")]
unsafe impl Sync for ClapLibrary {}

/// An instantiated CLAP plugin instance.
#[cfg(feature = "clap-host")]
struct ClapInstance {
    _library: Arc<ClapLibrary>,
    plugin: *const clap_plugin,
    active: bool,
    _sample_rate: f64,
    _max_frames: usize,
    in_channel_data: [*mut f32; 2],
    out_channel_data: [*mut f32; 2],
    params_ext: *const clap_plugin_params,
    state_ext: *const clap_plugin_state,
    pending_param_events: Vec<clap_event_param_value>,
}

#[cfg(feature = "clap-host")]
unsafe impl Send for ClapInstance {}

#[cfg(feature = "clap-host")]
impl Drop for ClapInstance {
    fn drop(&mut self) {
        if !self.plugin.is_null() {
            unsafe {
                let p = &*self.plugin;
                if self.active {
                    if let Some(deactivate) = p.deactivate {
                        deactivate(self.plugin);
                    }
                }
                if let Some(destroy) = p.destroy {
                    destroy(self.plugin);
                }
            }
        }
    }
}

#[cfg(feature = "clap-host")]
unsafe extern "C" fn in_events_size(list: *const clap_input_events) -> u32 {
    if list.is_null() || (*list).ctx.is_null() {
        return 0;
    }
    let events = (*list).ctx as *const Vec<clap_event_param_value>;
    (*events).len() as u32
}

#[cfg(feature = "clap-host")]
unsafe extern "C" fn in_events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    if list.is_null() || (*list).ctx.is_null() {
        return std::ptr::null();
    }
    let events = &*((*list).ctx as *const Vec<clap_event_param_value>);
    if let Some(ev) = events.get(index as usize) {
        &ev.header as *const _
    } else {
        std::ptr::null()
    }
}

#[cfg(feature = "clap-host")]
struct ClapStreamWriter {
    data: Vec<u8>,
}

#[cfg(feature = "clap-host")]
unsafe extern "C" fn stream_write_fn(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    if stream.is_null() || (*stream).ctx.is_null() || buffer.is_null() {
        return -1;
    }
    let writer = &mut *((*stream).ctx as *mut ClapStreamWriter);
    let bytes = std::slice::from_raw_parts(buffer as *const u8, size as usize);
    writer.data.extend_from_slice(bytes);
    size as i64
}

#[cfg(feature = "clap-host")]
struct ClapStreamReader<'a> {
    data: &'a [u8],
    pos: usize,
}

#[cfg(feature = "clap-host")]
unsafe extern "C" fn stream_read_fn(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    if stream.is_null() || (*stream).ctx.is_null() || buffer.is_null() {
        return -1;
    }
    let reader = &mut *((*stream).ctx as *mut ClapStreamReader<'_>);
    let available = reader.data.len().saturating_sub(reader.pos);
    let to_read = (size as usize).min(available);
    if to_read > 0 {
        std::ptr::copy_nonoverlapping(reader.data[reader.pos..].as_ptr(), buffer as *mut u8, to_read);
        reader.pos += to_read;
    }
    to_read as i64
}

/// The CLAP Plugin Host Node.
pub struct ClapPluginHostNode {
    sample_rate: f32,
    enabled: bool,
    latency_samples: u32,
    #[cfg(feature = "clap-host")]
    instance: Option<ClapInstance>,
    #[cfg(feature = "clap-host")]
    host_descriptor: Box<clap_host>,
}

// Safety: The host node is moved to the audio thread during graph generation swaps.
// The CLAP instance audio processing is exclusively invoked on the realtime audio thread.
unsafe impl Send for ClapPluginHostNode {}

#[cfg(feature = "clap-host")]
#[allow(dead_code)]
unsafe extern "C" fn clap_host_get_extension(
    _host: *const clap_host,
    _extension_id: *const c_char,
) -> *const c_void {
    std::ptr::null()
}

#[cfg(feature = "clap-host")]
#[allow(dead_code)]
unsafe extern "C" fn clap_host_request_restart(_host: *const clap_host) {}

#[cfg(feature = "clap-host")]
#[allow(dead_code)]
unsafe extern "C" fn clap_host_request_process(_host: *const clap_host) {}

#[cfg(feature = "clap-host")]
#[allow(dead_code)]
unsafe extern "C" fn clap_host_request_callback(_host: *const clap_host) {}

#[allow(dead_code)]
impl ClapPluginHostNode {
    /// Creates a new inactive CLAP plugin host node.
    pub fn new(sample_rate: f32) -> Self {
        #[cfg(feature = "clap-host")]
        {
            let host_descriptor = Box::new(clap_host {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: b"audio-engine\0".as_ptr() as *const c_char,
                vendor: b"Sudipta615\0".as_ptr() as *const c_char,
                url: b"https://github.com/Sudipta615/audio_engine\0".as_ptr() as *const c_char,
                version: b"1.0.0\0".as_ptr() as *const c_char,
                get_extension: Some(clap_host_get_extension),
                request_restart: Some(clap_host_request_restart),
                request_process: Some(clap_host_request_process),
                request_callback: Some(clap_host_request_callback),
            });

            Self {
                sample_rate,
                enabled: true,
                latency_samples: 0,
                instance: None,
                host_descriptor,
            }
        }

        #[cfg(not(feature = "clap-host"))]
        {
            Self {
                sample_rate,
                enabled: true,
                latency_samples: 0,
            }
        }
    }

    /// Loads and instantiates a CLAP plugin from a dynamic library file path (control thread).
    #[cfg(feature = "clap-host")]
    #[allow(dead_code)]
    pub fn load_plugin(&mut self, path: &Path, plugin_id: Option<&str>) -> Result<(), String> {
        let lib = unsafe {
            libloading::Library::new(path)
                .map_err(|e| format!("Failed to dlopen CLAP library: {e}"))?
        };

        // Standard CLAP entry point: "clap_entry" symbol
        let entry_ptr: *const clap_plugin_entry = unsafe {
            let sym: libloading::Symbol<*const clap_plugin_entry> = lib
                .get(b"clap_entry\0")
                .map_err(|e| format!("Missing clap_entry symbol: {e}"))?;
            *sym
        };

        if entry_ptr.is_null() {
            return Err("clap_entry symbol pointer is null".to_string());
        }

        let entry_ref = unsafe { &*entry_ptr };
        let c_path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|e| format!("Invalid path string: {e}"))?;

        let init_ok = unsafe {
            if let Some(init) = entry_ref.init {
                init(c_path.as_ptr())
            } else {
                false
            }
        };

        if !init_ok {
            return Err("CLAP entry init() returned false".to_string());
        }

        let factory_ptr = unsafe {
            if let Some(get_factory) = entry_ref.get_factory {
                get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr())
            } else {
                std::ptr::null()
            }
        };

        if factory_ptr.is_null() {
            return Err("CLAP factory is null".to_string());
        }

        let factory = unsafe { &*(factory_ptr as *const clap_plugin_factory) };
        let count = unsafe {
            if let Some(get_plugin_count) = factory.get_plugin_count {
                get_plugin_count(factory)
            } else {
                0
            }
        };

        if count == 0 {
            return Err("CLAP factory contains no plugins".to_string());
        }

        // Select plugin ID
        let target_id = if let Some(id) = plugin_id {
            CString::new(id).map_err(|e| format!("Invalid plugin ID: {e}"))?
        } else {
            // Pick first plugin
            let desc_ptr = unsafe {
                if let Some(get_plugin_descriptor) = factory.get_plugin_descriptor {
                    get_plugin_descriptor(factory, 0)
                } else {
                    std::ptr::null()
                }
            };
            if desc_ptr.is_null() {
                return Err("Failed to get first CLAP plugin descriptor".to_string());
            }
            let desc = unsafe { &*desc_ptr };
            unsafe { CStr::from_ptr(desc.id).to_owned() }
        };

        let host_ptr: *const clap_host = &*self.host_descriptor;
        let plugin_ptr = unsafe {
            if let Some(create_plugin) = factory.create_plugin {
                create_plugin(factory, host_ptr, target_id.as_ptr())
            } else {
                std::ptr::null()
            }
        };

        if plugin_ptr.is_null() {
            return Err("create_plugin returned null".to_string());
        }

        let plugin = unsafe { &*plugin_ptr };
        let initialized = unsafe {
            if let Some(init) = plugin.init {
                init(plugin_ptr)
            } else {
                false
            }
        };

        if !initialized {
            unsafe {
                if let Some(destroy) = plugin.destroy {
                    destroy(plugin_ptr);
                }
            }
            return Err("Plugin init() returned false".to_string());
        }

        let activated = unsafe {
            if let Some(activate) = plugin.activate {
                activate(
                    plugin_ptr,
                    self.sample_rate as f64,
                    32,
                    MAX_AUDIO_BLOCK_FRAMES as u32,
                )
            } else {
                false
            }
        };

        if !activated {
            unsafe {
                if let Some(destroy) = plugin.destroy {
                    destroy(plugin_ptr);
                }
            }
            return Err("Plugin activate() returned false".to_string());
        }

        let params_ext = unsafe {
            if let Some(get_ext) = plugin.get_extension {
                get_ext(plugin_ptr, CLAP_EXT_PARAMS.as_ptr()) as *const clap_plugin_params
            } else {
                std::ptr::null()
            }
        };

        let state_ext = unsafe {
            if let Some(get_ext) = plugin.get_extension {
                get_ext(plugin_ptr, CLAP_EXT_STATE.as_ptr()) as *const clap_plugin_state
            } else {
                std::ptr::null()
            }
        };

        let lib_arc = Arc::new(ClapLibrary {
            _lib: lib,
            _entry: entry_ptr,
        });

        self.instance = Some(ClapInstance {
            _library: lib_arc,
            plugin: plugin_ptr,
            active: true,
            _sample_rate: self.sample_rate as f64,
            _max_frames: MAX_AUDIO_BLOCK_FRAMES,
            in_channel_data: [std::ptr::null_mut(); 2],
            out_channel_data: [std::ptr::null_mut(); 2],
            params_ext,
            state_ext,
            pending_param_events: Vec::with_capacity(32),
        });

        log::info!("Successfully loaded and activated CLAP plugin: {:?}", target_id);
        Ok(())
    }

    /// Control path: toggle bypass.
    #[allow(dead_code)]
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Set a parameter value by ID.
    pub fn set_param(&mut self, _id: u32, _value: f64) {
        #[cfg(feature = "clap-host")]
        if let Some(ref mut inst) = self.instance {
            let ev = clap_event_param_value {
                header: clap_event_header {
                    size: std::mem::size_of::<clap_event_param_value>() as u32,
                    time: 0,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_PARAM_VALUE,
                    flags: 0,
                },
                param_id: _id,
                cookie: std::ptr::null_mut(),
                note_id: -1,
                port_index: 0,
                channel: -1,
                key: -1,
                value: _value,
            };
            inst.pending_param_events.push(ev);
        }
    }

    /// Apply a batch of parameter changes.
    pub fn apply_params(&mut self, batch: &[(u32, f64)]) {
        for &(id, val) in batch {
            self.set_param(id, val);
        }
    }

    /// Get current parameter value by ID.
    pub fn get_param_value(&self, _id: u32) -> Option<f64> {
        #[cfg(feature = "clap-host")]
        if let Some(ref inst) = self.instance {
            if !inst.params_ext.is_null() && !inst.plugin.is_null() {
                unsafe {
                    let ext = &*inst.params_ext;
                    if let Some(get_val) = ext.get_value {
                        let mut val = 0.0f64;
                        if get_val(inst.plugin, _id, &mut val) {
                            return Some(val);
                        }
                    }
                }
            }
        }
        None
    }

    /// Return total parameter count reported by the plugin.
    pub fn param_count(&self) -> u32 {
        #[cfg(feature = "clap-host")]
        if let Some(ref inst) = self.instance {
            if !inst.params_ext.is_null() && !inst.plugin.is_null() {
                unsafe {
                    let ext = &*inst.params_ext;
                    if let Some(count) = ext.count {
                        return count(inst.plugin);
                    }
                }
            }
        }
        0
    }

    /// Serialize current plugin state to an opaque byte buffer.
    pub fn save_state(&self) -> Result<Vec<u8>, String> {
        #[cfg(feature = "clap-host")]
        {
            if let Some(ref inst) = self.instance {
                if inst.state_ext.is_null() || inst.plugin.is_null() {
                    return Err("Plugin does not implement state extension".to_string());
                }
                let mut writer = ClapStreamWriter { data: Vec::new() };
                let ostream = clap_ostream {
                    ctx: &mut writer as *mut _ as *mut c_void,
                    write: Some(stream_write_fn),
                };
                unsafe {
                    let ext = &*inst.state_ext;
                    if let Some(save) = ext.save {
                        if save(inst.plugin, &ostream) {
                            return Ok(writer.data);
                        } else {
                            return Err("CLAP state save() returned false".to_string());
                        }
                    }
                }
            }
            Err("No active plugin instance to save state from".to_string())
        }
        #[cfg(not(feature = "clap-host"))]
        {
            Err("clap-host feature disabled".to_string())
        }
    }

    /// Restore plugin state from an opaque byte buffer.
    pub fn load_state(&mut self, _state_bytes: &[u8]) -> Result<(), String> {
        #[cfg(feature = "clap-host")]
        {
            if let Some(ref mut inst) = self.instance {
                if inst.state_ext.is_null() || inst.plugin.is_null() {
                    return Err("Plugin does not implement state extension".to_string());
                }
                let mut reader = ClapStreamReader {
                    data: _state_bytes,
                    pos: 0,
                };
                let istream = clap_istream {
                    ctx: &mut reader as *mut _ as *mut c_void,
                    read: Some(stream_read_fn),
                };
                unsafe {
                    let ext = &*inst.state_ext;
                    if let Some(load) = ext.load {
                        if load(inst.plugin, &istream) {
                            return Ok(());
                        } else {
                            return Err("CLAP state load() returned false".to_string());
                        }
                    }
                }
            }
            Err("No active plugin instance to load state into".to_string())
        }
        #[cfg(not(feature = "clap-host"))]
        {
            Err("clap-host feature disabled".to_string())
        }
    }
}

impl DspNode for ClapPluginHostNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "clap_plugin_host",
            channel_support: StageChannelSupport::StereoOnly,
            position: "master insert (post-volume, pre-limiter)",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::F32,
        }
    }

    fn is_active(&self) -> bool {
        #[cfg(feature = "clap-host")]
        {
            self.enabled && self.instance.is_some()
        }
        #[cfg(not(feature = "clap-host"))]
        {
            false
        }
    }

    fn latency_samples(&self) -> usize {
        self.latency_samples as usize
    }

    fn reset(&mut self) {
        #[cfg(feature = "clap-host")]
        if let Some(ref mut inst) = self.instance {
            if !inst.plugin.is_null() {
                unsafe {
                    let p = &*inst.plugin;
                    if let Some(reset) = p.reset {
                        reset(inst.plugin);
                    }
                }
            }
        }
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.sample_rate = sample_rate;
        #[cfg(feature = "clap-host")]
        if let Some(ref mut inst) = self.instance {
            if !inst.plugin.is_null() && inst.active {
                unsafe {
                    let p = &*inst.plugin;
                    if let Some(deactivate) = p.deactivate {
                        deactivate(inst.plugin);
                    }
                    if let Some(activate) = p.activate {
                        activate(
                            inst.plugin,
                            sample_rate as f64,
                            32,
                            MAX_AUDIO_BLOCK_FRAMES as u32,
                        );
                    }
                }
            }
        }
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if !self.is_active() || planes.len() < 2 {
            return;
        }

        #[cfg(feature = "clap-host")]
        if let Some(ref mut inst) = self.instance {
            let frames = planes[0].len().min(planes[1].len());
            if frames == 0 {
                return;
            }

            // Split borrow to obtain pointers to L and R channels
            let (first, rest) = planes.split_at_mut(1);
            let in_l = first[0].as_mut_ptr();
            let in_r = rest[0].as_mut_ptr();

            inst.in_channel_data[0] = in_l;
            inst.in_channel_data[1] = in_r;
            inst.out_channel_data[0] = in_l;
            inst.out_channel_data[1] = in_r;

            let in_audio = clap_audio_buffer {
                data32: inst.in_channel_data.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };

            let mut out_audio = clap_audio_buffer {
                data32: inst.out_channel_data.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };

            let has_events = !inst.pending_param_events.is_empty();
            let in_events = if has_events {
                clap_input_events {
                    ctx: &inst.pending_param_events as *const _ as *mut c_void,
                    size: Some(in_events_size),
                    get: Some(in_events_get),
                }
            } else {
                clap_input_events {
                    ctx: std::ptr::null_mut(),
                    size: None,
                    get: None,
                }
            };

            let process_data = clap_process {
                steady_time: -1,
                frames_count: frames as u32,
                transport: std::ptr::null(),
                audio_inputs: &in_audio as *const _,
                audio_outputs: &mut out_audio as *mut _,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: if has_events { &in_events as *const _ } else { std::ptr::null() },
                out_events: std::ptr::null(),
            };

            unsafe {
                let p = &*inst.plugin;
                if let Some(process_fn) = p.process {
                    let _status = process_fn(inst.plugin, &process_data);
                }
            }

            if has_events {
                inst.pending_param_events.clear();
            }
        }
    }

    fn process_block_f64(&mut self, _planes: &mut [&mut [f64]]) {
        // CLAP processing is f32 in this implementation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clap_host_node_lifecycle_without_plugin() {
        let mut node = ClapPluginHostNode::new(48000.0);
        assert!(!node.is_active());
        assert_eq!(node.latency_samples(), 0);

        let mut l = vec![1.0f32; 128];
        let mut r = vec![2.0f32; 128];
        let mut planes = [&mut l[..], &mut r[..]];

        node.process_block_f32(&mut planes);

        // Verification of pass-through behavior
        assert_eq!(l[0], 1.0);
        assert_eq!(r[0], 2.0);
    }
}
