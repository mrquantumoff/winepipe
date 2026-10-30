//! Driver configuration: `HKCU\Software\WinePipe`, overridable with
//! `WINEPIPE_*` environment variables.

use winepipe_abi as abi;

use crate::win::{Key, HKEY_CURRENT_USER};

pub const REG_PATH: &str = "Software\\WinePipe";

#[derive(Clone, Debug)]
pub struct Config {
    pub inputs: u32,
    pub outputs: u32,
    pub preferred_buffer_size: u32,
    pub fixed_buffer_size: bool,
    pub autoconnect: bool,
    pub sample_type: u32,
    pub client_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            inputs: 2,
            outputs: 2,
            preferred_buffer_size: 256,
            fixed_buffer_size: false,
            autoconnect: true,
            sample_type: abi::SAMPLE_FLOAT32,
            client_name: "WinePipe".into(),
        }
    }
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

impl Config {
    pub fn load() -> Config {
        let mut c = Config::default();
        if let Some(k) = Key::open(HKEY_CURRENT_USER, REG_PATH) {
            if let Some(v) = k.get_dword("Inputs") {
                c.inputs = v;
            }
            if let Some(v) = k.get_dword("Outputs") {
                c.outputs = v;
            }
            if let Some(v) = k.get_dword("PreferredBufferSize") {
                c.preferred_buffer_size = v;
            }
            if let Some(v) = k.get_dword("FixedBufferSize") {
                c.fixed_buffer_size = v != 0;
            }
            if let Some(v) = k.get_dword("AutoConnect") {
                c.autoconnect = v != 0;
            }
            if let Some(v) = k.get_dword("SampleType") {
                c.sample_type = v;
            }
            if let Some(v) = k.get_str("ClientName") {
                c.client_name = v;
            }
        }
        if let Some(v) = env_u32("WINEPIPE_INPUTS") {
            c.inputs = v;
        }
        if let Some(v) = env_u32("WINEPIPE_OUTPUTS") {
            c.outputs = v;
        }
        if let Some(v) = env_u32("WINEPIPE_BUFFER_SIZE") {
            c.preferred_buffer_size = v;
        }
        if let Some(v) = env_u32("WINEPIPE_FIXED_BUFFER_SIZE") {
            c.fixed_buffer_size = v != 0;
        }
        if let Some(v) = env_u32("WINEPIPE_AUTOCONNECT") {
            c.autoconnect = v != 0;
        }
        if let Some(v) = env_u32("WINEPIPE_SAMPLE_TYPE") {
            c.sample_type = v;
        }
        if let Ok(v) = std::env::var("WINEPIPE_CLIENT_NAME") {
            c.client_name = v;
        }
        c.sanitize();
        c
    }

    fn sanitize(&mut self) {
        self.inputs = self.inputs.min(abi::MAX_CHANNELS);
        self.outputs = self.outputs.min(abi::MAX_CHANNELS);
        self.preferred_buffer_size = self.preferred_buffer_size.clamp(abi::MIN_BUFFER_SIZE, abi::MAX_BUFFER_SIZE);
        if self.sample_type != abi::SAMPLE_INT32 {
            self.sample_type = abi::SAMPLE_FLOAT32;
        }
        self.client_name.retain(|c| !c.is_control());
        if self.client_name.trim().is_empty() {
            self.client_name = "WinePipe".into();
        }
    }
}
