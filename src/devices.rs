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
use windows::core::{GUID, HSTRING};

use crate::audio::channelmap::ChannelLayout;

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

    /// True when samples are 32-bit IEEE floats, which is what every WASAPI
    /// shared-mode mix uses and the only layout the passthrough path handles.
    pub fn is_f32(&self) -> bool {
        if self.bits_per_sample != 32 {
            return false;
        }
        match &self.extensible {
            Some(ext) => ext.sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            // 3 == WAVE_FORMAT_IEEE_FLOAT, for the non-extensible case.
            None => self.format_tag == 3,
        }
    }

    /// This format's speaker layout, for the channel adaptation stage.
    ///
    /// A missing extensible tail becomes a mask of zero, which is
    /// `KSAUDIO_SPEAKER_DIRECTOUT` — "no speaker assignment" — and is exactly
    /// how [`ChannelLayout`] wants an unknown layout expressed. The conversion
    /// lives here rather than in `channelmap` so that module stays free of any
    /// WASAPI type.
    pub fn channel_layout(&self) -> ChannelLayout {
        ChannelLayout::new(
            self.channels as usize,
            self.extensible.map_or(0, |ext| ext.channel_mask),
        )
    }

    /// Compact one-line rendering, used in "formats don't match" errors.
    pub fn summary(&self) -> String {
        let sample_type = if self.is_f32() { "f32" } else { "non-f32" };
        format!(
            "{} Hz, {} ch, {}-bit {} ({})",
            self.sample_rate,
            self.channels,
            self.bits_per_sample,
            sample_type,
            self.container_name()
        )
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

/// Look up a single render endpoint by its `IMMDevice::GetId()` string.
///
/// Used by the audio threads, which cannot be handed COM interfaces from the
/// main thread (the `windows` crate's interface types are deliberately not
/// `Send`) and so re-resolve their endpoint from the ID after doing their own
/// `CoInitializeEx`. The ID is the stable handle for exactly this reason.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread.
pub unsafe fn open_device_by_id(id: &str) -> Result<IMMDevice> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(MMDeviceEnumerator) failed")?;
        enumerator
            .GetDevice(&HSTRING::from(id))
            .with_context(|| format!("IMMDeviceEnumerator::GetDevice failed for id {id}"))
    }
}

/// Resolve a user-supplied `--source`/`--sink` argument against an enumeration.
///
/// Accepts either the bracketed index from the `list` report or a verbatim
/// device ID. Bare integers are tried as an index first; every real WASAPI ID
/// starts with `{`, so the two namespaces cannot collide.
pub fn resolve_spec<'a>(devices: &'a [DeviceInfo], spec: &str) -> Result<&'a DeviceInfo> {
    if let Ok(index) = spec.parse::<usize>() {
        return devices.get(index).with_context(|| {
            format!(
                "no endpoint with index {index}; `lockstep list` reported {} endpoint(s)",
                devices.len()
            )
        });
    }

    devices
        .iter()
        .find(|d| d.id.as_deref() == Some(spec))
        .with_context(|| format!("no render endpoint with id {spec}; run `lockstep list`"))
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

        let format = decode_wave_format(ptr);
        CoTaskMemFree(Some(ptr.cast()));
        Ok(format)
    }
}

/// Copy a `WAVEFORMATEX` — and its extensible tail, if present — into owned data.
///
/// Does not take ownership of `ptr`; freeing it stays the caller's job. The
/// audio threads use this to inspect the format they must hand straight back to
/// `IAudioClient::Initialize`, which needs the original allocation intact.
///
/// # Safety
///
/// `ptr` must point to a valid `WAVEFORMATEX` with at least `cbSize` further
/// bytes readable after it.
pub unsafe fn decode_wave_format(ptr: *const WAVEFORMATEX) -> MixFormat {
    unsafe {
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

        MixFormat {
            format_tag: base.wFormatTag,
            channels: base.nChannels,
            sample_rate: base.nSamplesPerSec,
            avg_bytes_per_sec: base.nAvgBytesPerSec,
            block_align: base.nBlockAlign,
            bits_per_sample: base.wBitsPerSample,
            cb_size: base.cbSize,
            extensible,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::KernelStreaming::{
        SPEAKER_BACK_CENTER, SPEAKER_TOP_FRONT_CENTER, SPEAKER_TOP_FRONT_LEFT,
    };

    // Channel masks the project actually cares about. The 5.1 and 7.1 values
    // are the answer to CLAUDE.md's open question about the HDMI endpoint.
    const MASK_STEREO: u32 = 0x3;
    const MASK_5_1: u32 = 0x3F;
    const MASK_7_1: u32 = 0x63F;

    fn extensible(mask: u32, sub_format: GUID) -> ExtensibleFormat {
        ExtensibleFormat {
            valid_bits_per_sample: 32,
            channel_mask: mask,
            sub_format,
        }
    }

    fn mix_format(
        sample_rate: u32,
        channels: u16,
        bits: u16,
        format_tag: u16,
        extensible: Option<ExtensibleFormat>,
    ) -> MixFormat {
        MixFormat {
            format_tag,
            channels,
            sample_rate,
            avg_bytes_per_sec: sample_rate * u32::from(channels) * u32::from(bits / 8),
            block_align: channels * (bits / 8),
            bits_per_sample: bits,
            cb_size: if extensible.is_some() { 22 } else { 0 },
            extensible,
        }
    }

    /// The shape every endpoint on the dev machine actually reports.
    fn shared_mode_f32(sample_rate: u32, channels: u16, mask: u32) -> MixFormat {
        mix_format(
            sample_rate,
            channels,
            32,
            0xFFFE,
            Some(extensible(mask, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)),
        )
    }

    fn device(
        index: usize,
        id: &str,
        name: &str,
        state: EndpointState,
        mix_format: Option<MixFormat>,
    ) -> DeviceInfo {
        DeviceInfo {
            index,
            id: Some(id.to_string()),
            friendly_name: Some(name.to_string()),
            state,
            mix_format,
            is_default_console: false,
            is_default_multimedia: false,
            errors: Vec::new(),
        }
    }

    /// Stands in for the dev machine: a stale absent entry sharing a friendly
    /// name with a live one, which is exactly why presets key on IDs.
    fn fixture() -> Vec<DeviceInfo> {
        vec![
            device(
                0,
                "{0.0.0.00000000}.{stale-tozo}",
                "Headphones (TOZO NC9)",
                EndpointState::NotPresent,
                None,
            ),
            device(
                1,
                "{0.0.0.00000000}.{monitor}",
                "H27T13 (Intel(R) Display Audio)",
                EndpointState::Active,
                Some(shared_mode_f32(48_000, 2, MASK_STEREO)),
            ),
            device(
                2,
                "{0.0.0.00000000}.{live-tozo}",
                "Headphones (TOZO NC9)",
                EndpointState::Active,
                Some(shared_mode_f32(48_000, 2, MASK_STEREO)),
            ),
        ]
    }

    #[test]
    fn endpoint_states_decode_and_name_themselves() {
        let cases = [
            (DEVICE_STATE_ACTIVE, EndpointState::Active, "Active"),
            (DEVICE_STATE_DISABLED, EndpointState::Disabled, "Disabled"),
            (
                DEVICE_STATE_NOTPRESENT,
                EndpointState::NotPresent,
                "NotPresent",
            ),
            (
                DEVICE_STATE_UNPLUGGED,
                EndpointState::Unplugged,
                "Unplugged",
            ),
        ];
        for (raw, expected, word) in cases {
            let state = EndpointState::from_raw(raw);
            assert_eq!(state, expected);
            assert_eq!(state.as_word(), word);
        }
    }

    #[test]
    fn an_unknown_state_bit_degrades_rather_than_panicking() {
        let state = EndpointState::from_raw(DEVICE_STATE(0x40));
        assert_eq!(state, EndpointState::Unknown(0x40));
        assert_eq!(state.as_word(), "Unknown");
        assert!(!state.is_active());
    }

    #[test]
    fn only_active_counts_as_active() {
        assert!(EndpointState::Active.is_active());
        for state in [
            EndpointState::Disabled,
            EndpointState::NotPresent,
            EndpointState::Unplugged,
            EndpointState::Unknown(0),
        ] {
            assert!(!state.is_active(), "{state:?} should not be active");
        }
    }

    #[test]
    fn stereo_channel_mask_decodes() {
        let ext = extensible(MASK_STEREO, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        assert_eq!(ext.speaker_names(), vec!["FL", "FR"]);
    }

    #[test]
    fn five_one_channel_mask_decodes_in_interleave_order() {
        let ext = extensible(MASK_5_1, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        assert_eq!(
            ext.speaker_names(),
            vec!["FL", "FR", "FC", "LFE", "BL", "BR"]
        );
    }

    #[test]
    fn seven_one_channel_mask_decodes_in_interleave_order() {
        let ext = extensible(MASK_7_1, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        assert_eq!(
            ext.speaker_names(),
            vec!["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]
        );
    }

    #[test]
    fn an_empty_channel_mask_names_no_speakers() {
        let ext = extensible(0, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        assert!(ext.speaker_names().is_empty());
    }

    #[test]
    fn high_top_speaker_bits_decode() {
        let mask = SPEAKER_TOP_FRONT_LEFT | SPEAKER_TOP_FRONT_CENTER | SPEAKER_BACK_CENTER;
        let ext = extensible(mask, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        assert_eq!(ext.speaker_names(), vec!["BC", "TFL", "TFC"]);
    }

    #[test]
    fn undefined_mask_bits_are_ignored() {
        // Bit 31 is not a defined speaker position; it must not appear or panic.
        let ext = extensible(MASK_STEREO | 0x8000_0000, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        assert_eq!(ext.speaker_names(), vec!["FL", "FR"]);
    }

    #[test]
    fn sub_format_guids_are_identified() {
        assert_eq!(
            extensible(MASK_STEREO, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT).sub_format_name(),
            "KSDATAFORMAT_SUBTYPE_IEEE_FLOAT"
        );
        assert_eq!(
            extensible(MASK_STEREO, KSDATAFORMAT_SUBTYPE_PCM).sub_format_name(),
            "KSDATAFORMAT_SUBTYPE_PCM"
        );
        assert_eq!(
            extensible(MASK_STEREO, GUID::from_u128(0xdead_beef)).sub_format_name(),
            "unrecognized SubFormat GUID"
        );
    }

    #[test]
    fn container_names_cover_the_tags_we_expect() {
        assert_eq!(
            shared_mode_f32(48_000, 2, MASK_STEREO).container_name(),
            "WAVE_FORMAT_EXTENSIBLE"
        );
        assert_eq!(
            mix_format(48_000, 2, 16, 1, None).container_name(),
            "WAVE_FORMAT_PCM"
        );
        assert_eq!(
            mix_format(48_000, 2, 32, 3, None).container_name(),
            "WAVE_FORMAT_IEEE_FLOAT"
        );
        assert_eq!(
            mix_format(48_000, 2, 32, 0x1234, None).container_name(),
            "unrecognized wFormatTag"
        );
    }

    #[test]
    fn extensible_float_is_f32() {
        assert!(shared_mode_f32(48_000, 2, MASK_STEREO).is_f32());
    }

    #[test]
    fn plain_ieee_float_without_a_tail_is_f32() {
        assert!(mix_format(48_000, 2, 32, 3, None).is_f32());
    }

    #[test]
    fn extensible_pcm_is_not_f32() {
        // 32 bits wide but integer samples — the passthrough path must refuse
        // this rather than reinterpret the bits as floats.
        let format = mix_format(
            48_000,
            2,
            32,
            0xFFFE,
            Some(extensible(MASK_STEREO, KSDATAFORMAT_SUBTYPE_PCM)),
        );
        assert!(!format.is_f32());
    }

    #[test]
    fn sixteen_bit_is_not_f32() {
        assert!(!mix_format(48_000, 2, 16, 1, None).is_f32());
        // Even when the tag claims float, the width has the final say.
        assert!(!mix_format(48_000, 2, 16, 3, None).is_f32());
    }

    #[test]
    fn a_mix_format_hands_its_layout_to_the_channel_mapper() {
        let layout = shared_mode_f32(48_000, 6, MASK_5_1).channel_layout();
        assert_eq!(layout.channels(), 6);
        assert_eq!(layout.mask(), MASK_5_1);
    }

    #[test]
    fn a_format_with_no_extensible_tail_reports_an_unassigned_layout() {
        // Mask zero is KSAUDIO_SPEAKER_DIRECTOUT, which is what the adapter
        // treats as "positional order is all we know".
        let layout = mix_format(48_000, 2, 32, 3, None).channel_layout();
        assert_eq!(layout.channels(), 2);
        assert_eq!(layout.mask(), 0);
    }

    #[test]
    fn summary_reports_the_facts_an_error_message_needs() {
        let summary = shared_mode_f32(48_000, 2, MASK_STEREO).summary();
        assert!(summary.contains("48000 Hz"), "{summary}");
        assert!(summary.contains("2 ch"), "{summary}");
        assert!(summary.contains("32-bit"), "{summary}");
        assert!(summary.contains("f32"), "{summary}");
        assert!(summary.contains("WAVE_FORMAT_EXTENSIBLE"), "{summary}");

        let pcm = mix_format(44_100, 6, 16, 1, None).summary();
        assert!(pcm.contains("44100 Hz"), "{pcm}");
        assert!(pcm.contains("6 ch"), "{pcm}");
        assert!(pcm.contains("non-f32"), "{pcm}");
    }

    #[test]
    fn resolve_spec_finds_a_device_by_index() {
        let devices = fixture();
        let found = resolve_spec(&devices, "1").expect("index 1 resolves");
        assert_eq!(found.index, 1);
        assert_eq!(found.id.as_deref(), Some("{0.0.0.00000000}.{monitor}"));
    }

    #[test]
    fn resolve_spec_finds_a_device_by_verbatim_id() {
        let devices = fixture();
        let found =
            resolve_spec(&devices, "{0.0.0.00000000}.{live-tozo}").expect("the id resolves");
        assert_eq!(found.index, 2);
    }

    #[test]
    fn an_id_disambiguates_devices_sharing_a_friendly_name() {
        // Two endpoints called "Headphones (TOZO NC9)"; only the ID separates
        // them. This is the whole reason presets key on IDs.
        let devices = fixture();
        let stale = resolve_spec(&devices, "{0.0.0.00000000}.{stale-tozo}").unwrap();
        let live = resolve_spec(&devices, "{0.0.0.00000000}.{live-tozo}").unwrap();
        assert_eq!(stale.friendly_name, live.friendly_name);
        assert_ne!(stale.index, live.index);
        assert!(!stale.state.is_active());
        assert!(live.state.is_active());
    }

    #[test]
    fn resolve_spec_rejects_an_index_past_the_end() {
        let devices = fixture();
        let error = resolve_spec(&devices, "99").expect_err("99 is out of range");
        let message = format!("{error}");
        assert!(message.contains("99"), "{message}");
        assert!(message.contains('3'), "{message}");
    }

    #[test]
    fn a_numeric_spec_never_falls_through_to_id_matching() {
        // "0" parses as an index, so it resolves positionally even though no
        // device ID looks like that. Documents the precedence deliberately.
        let devices = fixture();
        assert_eq!(resolve_spec(&devices, "0").unwrap().index, 0);
    }

    #[test]
    fn resolve_spec_rejects_an_unknown_id() {
        let devices = fixture();
        let error = resolve_spec(&devices, "{0.0.0.00000000}.{nope}").expect_err("no such id");
        assert!(format!("{error}").contains("lockstep list"));
    }

    #[test]
    fn resolve_spec_on_an_empty_list_fails_cleanly() {
        assert!(resolve_spec(&[], "0").is_err());
        assert!(resolve_spec(&[], "{some-id}").is_err());
    }

    #[test]
    fn resolve_spec_skips_devices_with_no_id() {
        let mut devices = fixture();
        devices[1].id = None;
        // Index lookup still works; ID lookup for the missing one cannot match.
        assert_eq!(resolve_spec(&devices, "1").unwrap().index, 1);
        assert!(resolve_spec(&devices, "{0.0.0.00000000}.{monitor}").is_err());
    }
}
