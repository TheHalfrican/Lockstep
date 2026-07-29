//! WASAPI render-endpoint discovery.
//!
//! This module owns every COM call needed to answer "what output devices exist,
//! what are they called, and what format does WASAPI want to talk to them in?".
//! It deliberately produces plain owned Rust data ([`DeviceInfo`]) rather than
//! handing COM interfaces back to callers, so the rest of the application — GUI
//! thread included — can hold on to the results without any apartment or
//! lifetime concerns. Later milestones will grow this module into the full
//! device-management layer (activation, `IMMNotificationClient` hotplug
//! handling, re-resolving preset IDs), which is why the data model is separated
//! from presentation: nothing here prints.

use anyhow::{Context, Result};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, DEVICE_STATE_NOTPRESENT,
    DEVICE_STATE_UNPLUGGED, DEVICE_STATEMASK_ALL, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eMultimedia, eRender,
};
use windows::Win32::Media::KernelStreaming::{
    KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_BACK_CENTER, SPEAKER_BACK_LEFT, SPEAKER_BACK_RIGHT,
    SPEAKER_FRONT_CENTER, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_LEFT_OF_CENTER, SPEAKER_FRONT_RIGHT,
    SPEAKER_FRONT_RIGHT_OF_CENTER, SPEAKER_LOW_FREQUENCY, SPEAKER_SIDE_LEFT, SPEAKER_SIDE_RIGHT,
    SPEAKER_TOP_BACK_CENTER, SPEAKER_TOP_BACK_LEFT, SPEAKER_TOP_BACK_RIGHT, SPEAKER_TOP_CENTER,
    SPEAKER_TOP_FRONT_CENTER, SPEAKER_TOP_FRONT_LEFT, SPEAKER_TOP_FRONT_RIGHT,
    WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::core::GUID;

/// Endpoint lifecycle state, as reported by `IMMDevice::GetState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointState {
    Active,
    Disabled,
    NotPresent,
    Unplugged,
    /// A bit pattern `mmdevapi` grew after this was written.
    Unknown(u32),
}

impl EndpointState {
    fn from_raw(state: DEVICE_STATE) -> Self {
        match state {
            DEVICE_STATE_ACTIVE => EndpointState::Active,
            DEVICE_STATE_DISABLED => EndpointState::Disabled,
            DEVICE_STATE_NOTPRESENT => EndpointState::NotPresent,
            DEVICE_STATE_UNPLUGGED => EndpointState::Unplugged,
            other => EndpointState::Unknown(other.0),
        }
    }

    /// Human-readable one-word rendering for reports and the eventual UI.
    pub fn as_word(&self) -> &'static str {
        match self {
            EndpointState::Active => "Active",
            EndpointState::Disabled => "Disabled",
            EndpointState::NotPresent => "NotPresent",
            EndpointState::Unplugged => "Unplugged",
            EndpointState::Unknown(_) => "Unknown",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, EndpointState::Active)
    }
}

/// The `WAVEFORMATEXTENSIBLE` tail, present only when `format_tag == 0xFFFE`.
#[derive(Debug, Clone, Copy)]
pub struct ExtensibleFormat {
    pub valid_bits_per_sample: u16,
    pub channel_mask: u32,
    pub sub_format: GUID,
}

impl ExtensibleFormat {
    /// Name of the sample encoding behind the `SubFormat` GUID.
    ///
    /// Only float and PCM matter for this project: WASAPI shared mode always
    /// hands us 32-bit float, and anything else showing up here means the
    /// endpoint is doing something we do not yet handle.
    pub fn sub_format_name(&self) -> &'static str {
        if self.sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            "KSDATAFORMAT_SUBTYPE_IEEE_FLOAT"
        } else if self.sub_format == KSDATAFORMAT_SUBTYPE_PCM {
            "KSDATAFORMAT_SUBTYPE_PCM"
        } else {
            "unrecognized SubFormat GUID"
        }
    }

    /// Speaker positions encoded in `dwChannelMask`, in the canonical
    /// interleave order the mask bits are defined in.
    ///
    /// This is the output that answers the project's open question about
    /// whether the HDMI/eARC endpoint negotiates multichannel LPCM: a mask of
    /// `0x3` is a stereo endpoint and the downmix matrix can be dropped, while
    /// `0x3F` (5.1) or `0x63F` (7.1) means it is required.
    pub fn speaker_names(&self) -> Vec<&'static str> {
        const SPEAKERS: &[(u32, &str)] = &[
            (SPEAKER_FRONT_LEFT, "FL"),
            (SPEAKER_FRONT_RIGHT, "FR"),
            (SPEAKER_FRONT_CENTER, "FC"),
            (SPEAKER_LOW_FREQUENCY, "LFE"),
            (SPEAKER_BACK_LEFT, "BL"),
            (SPEAKER_BACK_RIGHT, "BR"),
            (SPEAKER_FRONT_LEFT_OF_CENTER, "FLC"),
            (SPEAKER_FRONT_RIGHT_OF_CENTER, "FRC"),
            (SPEAKER_BACK_CENTER, "BC"),
            (SPEAKER_SIDE_LEFT, "SL"),
            (SPEAKER_SIDE_RIGHT, "SR"),
            (SPEAKER_TOP_CENTER, "TC"),
            (SPEAKER_TOP_FRONT_LEFT, "TFL"),
            (SPEAKER_TOP_FRONT_CENTER, "TFC"),
            (SPEAKER_TOP_FRONT_RIGHT, "TFR"),
            (SPEAKER_TOP_BACK_LEFT, "TBL"),
            (SPEAKER_TOP_BACK_CENTER, "TBC"),
            (SPEAKER_TOP_BACK_RIGHT, "TBR"),
        ];

        SPEAKERS
            .iter()
            .filter(|(bit, _)| self.channel_mask & bit != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

/// The shared-mode mix format WASAPI will accept without conversion.
#[derive(Debug, Clone, Copy)]
pub struct MixFormat {
    pub format_tag: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub avg_bytes_per_sec: u32,
    pub block_align: u16,
    /// Container size in bits — not necessarily the number of meaningful bits;
    /// see [`ExtensibleFormat::valid_bits_per_sample`].
    pub bits_per_sample: u16,
    pub cb_size: u16,
    pub extensible: Option<ExtensibleFormat>,
}

impl MixFormat {
    /// Name of the `wFormatTag` container, independent of the sub-format.
    pub fn container_name(&self) -> &'static str {
        // 1 = WAVE_FORMAT_PCM, 3 = WAVE_FORMAT_IEEE_FLOAT, 0xFFFE = EXTENSIBLE.
        match u32::from(self.format_tag) {
            1 => "WAVE_FORMAT_PCM",
            3 => "WAVE_FORMAT_IEEE_FLOAT",
            WAVE_FORMAT_EXTENSIBLE => "WAVE_FORMAT_EXTENSIBLE",
            _ => "unrecognized wFormatTag",
        }
    }
}

/// Everything known about one render endpoint.
///
/// `id` is the value presets are keyed on. Friendly names are display-only:
/// with two Astro base stations attached they collide.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Position in the enumeration, purely for referring to a device in output.
    pub index: usize,
    /// `IMMDevice::GetId()` verbatim. Stable across reboots and renames.
    pub id: Option<String>,
    pub friendly_name: Option<String>,
    pub state: EndpointState,
    /// Populated for active devices only; inactive endpoints cannot be
    /// activated and `GetMixFormat` would fail on them.
    pub mix_format: Option<MixFormat>,
    pub is_default_console: bool,
    pub is_default_multimedia: bool,
    /// Per-device failures. A broken endpoint degrades its own entry rather
    /// than aborting the whole enumeration.
    pub errors: Vec<String>,
}

/// Enumerate every render endpoint in every device state.
///
/// # COM
///
/// The calling thread must already have called `CoInitializeEx`. This function
/// does not initialize COM itself because apartment state is per-thread and
/// owned by whoever spawned the thread.
pub fn enumerate_render_endpoints() -> Result<Vec<DeviceInfo>> {
    // SAFETY: COM is initialized by the caller; the enumerator is a standard
    // in-proc server and every raw pointer below is checked before use.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(MMDeviceEnumerator) failed")?;

        // Default endpoints are looked up by ID so they can be matched against
        // the enumerated list. Failure here is not fatal — a machine with no
        // audio devices at all legitimately returns E_NOTFOUND.
        let default_console = default_endpoint_id(&enumerator, eConsole);
        let default_multimedia = default_endpoint_id(&enumerator, eMultimedia);

        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE(DEVICE_STATEMASK_ALL))
            .context("IMMDeviceEnumerator::EnumAudioEndpoints(eRender, ALL) failed")?;

        let count = collection
            .GetCount()
            .context("IMMDeviceCollection::GetCount failed")?;

        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let idx = index as usize;
            match collection.Item(index) {
                Ok(device) => devices.push(inspect_device(
                    &device,
                    idx,
                    default_console.as_deref(),
                    default_multimedia.as_deref(),
                )),
                Err(err) => devices.push(DeviceInfo {
                    index: idx,
                    id: None,
                    friendly_name: None,
                    state: EndpointState::Unknown(0),
                    mix_format: None,
                    is_default_console: false,
                    is_default_multimedia: false,
                    errors: vec![format!(
                        "device #{idx}: IMMDeviceCollection::Item({index}) failed: {err}"
                    )],
                }),
            }
        }

        Ok(devices)
    }
}

/// Resolve the default endpoint for `role` to its device ID string.
///
/// Returns `None` (rather than an error) when there is no default endpoint for
/// the role, which is a normal condition on a machine with no audio hardware.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread.
unsafe fn default_endpoint_id(
    enumerator: &IMMDeviceEnumerator,
    role: windows::Win32::Media::Audio::ERole,
) -> Option<String> {
    unsafe {
        let device = enumerator.GetDefaultAudioEndpoint(eRender, role).ok()?;
        device_id(&device).ok()
    }
}

/// Gather one device's information, recording rather than propagating failures.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread and `device` must be live.
unsafe fn inspect_device(
    device: &IMMDevice,
    index: usize,
    default_console: Option<&str>,
    default_multimedia: Option<&str>,
) -> DeviceInfo {
    unsafe {
        let mut errors: Vec<String> = Vec::new();

        let id = match device_id(device) {
            Ok(id) => Some(id),
            Err(err) => {
                errors.push(format!("device #{index}: IMMDevice::GetId failed: {err:#}"));
                None
            }
        };

        // The name is fetched before anything else that could fail so later
        // error messages can identify the device by name, not just by index.
        let friendly_name = match friendly_name(device) {
            Ok(name) => Some(name),
            Err(err) => {
                errors.push(format!(
                    "device #{index}: friendly name lookup failed: {err:#}"
                ));
                None
            }
        };

        let label = friendly_name.as_deref().unwrap_or("<unnamed>");

        let state = match device.GetState() {
            Ok(state) => EndpointState::from_raw(state),
            Err(err) => {
                errors.push(format!(
                    "device #{index} ({label}): IMMDevice::GetState failed: {err}"
                ));
                EndpointState::Unknown(0)
            }
        };

        // Only active endpoints can be activated. Asking an unplugged or
        // disabled endpoint for a mix format returns an error that is expected,
        // not interesting, so it is not attempted.
        let mix_format = if state.is_active() {
            match mix_format(device) {
                Ok(format) => Some(format),
                Err(err) => {
                    errors.push(format!("device #{index} ({label}): {err:#}"));
                    None
                }
            }
        } else {
            None
        };

        let is_default_console =
            matches!((id.as_deref(), default_console), (Some(a), Some(b)) if a == b);
        let is_default_multimedia =
            matches!((id.as_deref(), default_multimedia), (Some(a), Some(b)) if a == b);

        DeviceInfo {
            index,
            id,
            friendly_name,
            state,
            mix_format,
            is_default_console,
            is_default_multimedia,
            errors,
        }
    }
}

/// Read `IMMDevice::GetId` into an owned `String`.
///
/// # COM memory
///
/// `GetId` allocates the returned string with `CoTaskMemAlloc` and transfers
/// ownership to us. The `windows` crate hands it back as a bare `PWSTR` with no
/// destructor attached, so it must be copied into a Rust `String` and then
/// released with `CoTaskMemFree` or it leaks for the process lifetime.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread and `device` must be live.
unsafe fn device_id(device: &IMMDevice) -> Result<String> {
    unsafe {
        let pwstr = device.GetId().context("IMMDevice::GetId failed")?;
        if pwstr.is_null() {
            anyhow::bail!("IMMDevice::GetId returned a null string");
        }
        let id = pwstr.to_string();
        CoTaskMemFree(Some(pwstr.as_ptr().cast()));
        id.context("IMMDevice::GetId returned invalid UTF-16")
    }
}

/// Read `PKEY_Device_FriendlyName` from the endpoint's property store.
///
/// # COM memory
///
/// `IPropertyStore::GetValue` fills a `PROPVARIANT` that owns a
/// `CoTaskMemAlloc`'d string. The `windows` crate's `PROPVARIANT` here is the
/// raw ABI struct with no `Drop`, so the value is copied out *before*
/// `PropVariantClear` releases it — reading afterwards would be a use-after-free.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread and `device` must be live.
unsafe fn friendly_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device
            .OpenPropertyStore(STGM_READ)
            .context("IMMDevice::OpenPropertyStore(STGM_READ) failed")?;

        let mut prop: PROPVARIANT = store
            .GetValue(&PKEY_Device_FriendlyName)
            .context("IPropertyStore::GetValue(PKEY_Device_FriendlyName) failed")?;

        let variant = &prop.Anonymous.Anonymous;
        let name = if variant.vt == VT_LPWSTR {
            let pwsz = variant.Anonymous.pwszVal;
            if pwsz.is_null() {
                Err(anyhow::anyhow!(
                    "PKEY_Device_FriendlyName held a null VT_LPWSTR"
                ))
            } else {
                pwsz.to_string()
                    .context("PKEY_Device_FriendlyName was not valid UTF-16")
            }
        } else {
            Err(anyhow::anyhow!(
                "PKEY_Device_FriendlyName had unexpected variant type {}",
                variant.vt.0
            ))
        };

        // Cleared unconditionally: even the error paths above have a live
        // allocation to release.
        let _ = PropVariantClear(&mut prop);
        name
    }
}

/// Activate an `IAudioClient` and read the shared-mode mix format.
///
/// # COM memory
///
/// `GetMixFormat` returns a `CoTaskMemAlloc`'d `WAVEFORMATEX` (plus tail bytes
/// for the extensible case). Everything needed is copied into an owned
/// [`MixFormat`] and the pointer is freed with `CoTaskMemFree` before returning.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread and `device` must be live and
/// active.
unsafe fn mix_format(device: &IMMDevice) -> Result<MixFormat> {
    unsafe {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("IMMDevice::Activate(IAudioClient) failed")?;

        let ptr = client
            .GetMixFormat()
            .context("IAudioClient::GetMixFormat failed")?;
        if ptr.is_null() {
            anyhow::bail!("IAudioClient::GetMixFormat returned a null format");
        }

        // WAVEFORMATEX is `#[repr(packed)]`, so it is read as a whole and its
        // fields copied out by value; taking a reference to a packed field is
        // not allowed.
        let base: WAVEFORMATEX = std::ptr::read_unaligned(ptr);

        // The extensible tail is only present when the tag says so *and* the
        // driver actually appended the extra 22 bytes.
        let extensible =
            if u32::from(base.wFormatTag) == WAVE_FORMAT_EXTENSIBLE && base.cbSize >= 22 {
                let ext: WAVEFORMATEXTENSIBLE = std::ptr::read_unaligned(ptr.cast());
                Some(ExtensibleFormat {
                    valid_bits_per_sample: ext.Samples.wValidBitsPerSample,
                    channel_mask: ext.dwChannelMask,
                    sub_format: ext.SubFormat,
                })
            } else {
                None
            };

        CoTaskMemFree(Some(ptr.cast()));

        Ok(MixFormat {
            format_tag: base.wFormatTag,
            channels: base.nChannels,
            sample_rate: base.nSamplesPerSec,
            avg_bytes_per_sec: base.nAvgBytesPerSec,
            block_align: base.nBlockAlign,
            bits_per_sample: base.wBitsPerSample,
            cb_size: base.cbSize,
            extensible,
        })
    }
}
