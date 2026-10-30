//! ASIO SDK types (Windows layout). Shared with the test host via `#[path]`.

#![allow(non_snake_case, non_upper_case_globals, dead_code)]

use std::ffi::c_void;

pub type ASIOBool = i32;
pub type ASIOError = i32;

pub const ASE_OK: ASIOError = 0;
pub const ASE_SUCCESS: ASIOError = 0x3f48_47a0;
pub const ASE_NotPresent: ASIOError = -1000;
pub const ASE_HWMalfunction: ASIOError = -999;
pub const ASE_InvalidParameter: ASIOError = -998;
pub const ASE_InvalidMode: ASIOError = -997;
pub const ASE_SPNotAdvancing: ASIOError = -996;
pub const ASE_NoClock: ASIOError = -995;
pub const ASE_NoMemory: ASIOError = -994;

pub const ASIOSTInt32LSB: i32 = 18;
pub const ASIOSTFloat32LSB: i32 = 19;

// asioMessage selectors
pub const kAsioSelectorSupported: i32 = 1;
pub const kAsioEngineVersion: i32 = 2;
pub const kAsioResetRequest: i32 = 3;
pub const kAsioBufferSizeChange: i32 = 4;
pub const kAsioResyncRequest: i32 = 5;
pub const kAsioLatenciesChanged: i32 = 6;
pub const kAsioSupportsTimeInfo: i32 = 7;
pub const kAsioSupportsTimeCode: i32 = 8;
pub const kAsioOverload: i32 = 15;

// future() selectors
pub const kAsioEnableTimeCodeRead: i32 = 1;
pub const kAsioDisableTimeCodeRead: i32 = 2;
pub const kAsioSetInputMonitor: i32 = 3;
pub const kAsioTransport: i32 = 4;
pub const kAsioCanInputMonitor: i32 = 9;
pub const kAsioCanTimeInfo: i32 = 10;
pub const kAsioCanTimeCode: i32 = 11;
pub const kAsioCanTransport: i32 = 12;
pub const kAsioCanReportOverload: i32 = 0x2404_2012;
pub const kAsioGetInternalBufferSamples: i32 = 0x2504_2012;

// AsioTimeInfo flags
pub const kSystemTimeValid: u32 = 1;
pub const kSamplePositionValid: u32 = 2;
pub const kSampleRateValid: u32 = 4;
pub const kSpeedValid: u32 = 8;
pub const kSampleRateChanged: u32 = 16;

/// 64-bit value split ASIO-style (`hi` first).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ASIOSamples {
    pub hi: u32,
    pub lo: u32,
}

impl ASIOSamples {
    pub fn from_u64(v: u64) -> Self {
        ASIOSamples { hi: (v >> 32) as u32, lo: v as u32 }
    }
    pub fn to_u64(self) -> u64 {
        (u64::from(self.hi) << 32) | u64::from(self.lo)
    }
}

pub type ASIOTimeStamp = ASIOSamples;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOClockSource {
    pub index: i32,
    pub associatedChannel: i32,
    pub associatedGroup: i32,
    pub isCurrentSource: ASIOBool,
    pub name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOChannelInfo {
    pub channel: i32,
    pub isInput: ASIOBool,
    pub isActive: ASIOBool,
    pub channelGroup: i32,
    pub type_: i32,
    pub name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOBufferInfo {
    pub isInput: ASIOBool,
    pub channelNum: i32,
    pub buffers: [*mut c_void; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AsioTimeInfo {
    pub speed: f64,
    pub systemTime: ASIOTimeStamp,
    pub samplePosition: ASIOSamples,
    pub sampleRate: f64,
    pub flags: u32,
    pub reserved: [u8; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOTimeCode {
    pub speed: f64,
    pub timeCodeSamples: ASIOSamples,
    pub flags: u32,
    pub future: [u8; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOTime {
    pub reserved: [i32; 4],
    pub timeInfo: AsioTimeInfo,
    pub timeCode: ASIOTimeCode,
}

impl Default for ASIOTime {
    fn default() -> Self {
        ASIOTime {
            reserved: [0; 4],
            timeInfo: AsioTimeInfo::default(),
            timeCode: ASIOTimeCode { speed: 0.0, timeCodeSamples: ASIOSamples::default(), flags: 0, future: [0; 64] },
        }
    }
}

/// Host callbacks (plain cdecl function pointers in the SDK).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOCallbacks {
    pub bufferSwitch: Option<unsafe extern "C" fn(index: i32, direct: ASIOBool)>,
    pub sampleRateDidChange: Option<unsafe extern "C" fn(rate: f64)>,
    pub asioMessage: Option<unsafe extern "C" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32>,
    pub bufferSwitchTimeInfo: Option<unsafe extern "C" fn(params: *mut ASIOTime, index: i32, direct: ASIOBool) -> *mut ASIOTime>,
}

const _: () = {
    use std::mem::size_of;
    assert!(size_of::<AsioTimeInfo>() == 48);
    assert!(size_of::<ASIOTimeCode>() == 88);
    assert!(size_of::<ASIOTime>() == 152);
    assert!(size_of::<ASIOChannelInfo>() == 52);
    assert!(size_of::<ASIOClockSource>() == 48);
    assert!(size_of::<ASIOBufferInfo>() == 8 + 2 * size_of::<usize>());
};
