#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

use std::{
    ffi::{c_void, CString},
    mem::size_of,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
};

use coreaudio_sys::*;
use m5mic_virtual_mic::{VirtualMicReader, CHANNELS, SAMPLE_RATE};

const DEVICE_ID: AudioObjectID = 2;
const STREAM_ID: AudioObjectID = 3;
const DRIVER_UID: &str = "com.nicosuave.m5mic.driver";
const DEVICE_UID: &str = "com.nicosuave.m5mic.device";
const MODEL_UID: &str = "com.nicosuave.m5mic.model";
const DEVICE_NAME: &str = "m5mic";
const MANUFACTURER: &str = "M5Stack";
const BUFFER_FRAME_SIZE: u32 = 512;

const AUDIO_SERVER_PLUGIN_TYPE_UUID: [u8; 16] = [
    0x44, 0x3a, 0xba, 0xb8, 0xe7, 0xb3, 0x49, 0x1a, 0xb9, 0x85, 0xbe, 0xb9, 0x18, 0x70, 0x30, 0xdb,
];
const AUDIO_SERVER_PLUGIN_DRIVER_INTERFACE_UUID: [u8; 16] = [
    0xee, 0xa5, 0x77, 0x3d, 0xcc, 0x43, 0x49, 0xf1, 0x8e, 0x00, 0x8f, 0x96, 0xe7, 0xd2, 0x3b, 0x17,
];
const IUNKNOWN_UUID: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

static REF_COUNT: AtomicU32 = AtomicU32::new(1);
static IO_RUNNING: AtomicBool = AtomicBool::new(false);
static SAMPLE_TIME: AtomicU64 = AtomicU64::new(0);
static ZERO_TIME_SEED: AtomicU64 = AtomicU64::new(1);
static READER: AtomicPtr<VirtualMicReader> = AtomicPtr::new(ptr::null_mut());

static mut DRIVER_INTERFACE_PTR: *mut AudioServerPlugInDriverInterface = ptr::null_mut();
static mut DRIVER_INTERFACE: AudioServerPlugInDriverInterface = AudioServerPlugInDriverInterface {
    _reserved: ptr::null_mut(),
    QueryInterface: Some(query_interface),
    AddRef: Some(add_ref),
    Release: Some(release),
    Initialize: Some(initialize),
    CreateDevice: Some(create_device),
    DestroyDevice: Some(destroy_device),
    AddDeviceClient: Some(add_device_client),
    RemoveDeviceClient: Some(remove_device_client),
    PerformDeviceConfigurationChange: Some(perform_device_configuration_change),
    AbortDeviceConfigurationChange: Some(abort_device_configuration_change),
    HasProperty: Some(has_property),
    IsPropertySettable: Some(is_property_settable),
    GetPropertyDataSize: Some(get_property_data_size),
    GetPropertyData: Some(get_property_data),
    SetPropertyData: Some(set_property_data),
    StartIO: Some(start_io),
    StopIO: Some(stop_io),
    GetZeroTimeStamp: Some(get_zero_time_stamp),
    WillDoIOOperation: Some(will_do_io_operation),
    BeginIOOperation: Some(begin_io_operation),
    DoIOOperation: Some(do_io_operation),
    EndIOOperation: Some(end_io_operation),
};

#[no_mangle]
pub unsafe extern "C" fn M5Mic_Create(
    _allocator: CFAllocatorRef,
    type_uuid: CFUUIDRef,
) -> *mut c_void {
    if !cf_uuid_equals(type_uuid, AUDIO_SERVER_PLUGIN_TYPE_UUID) {
        return ptr::null_mut();
    }

    DRIVER_INTERFACE_PTR = ptr::addr_of_mut!(DRIVER_INTERFACE);
    ptr::addr_of_mut!(DRIVER_INTERFACE_PTR).cast::<c_void>()
}

unsafe extern "C" fn query_interface(
    driver: *mut c_void,
    uuid: REFIID,
    out_interface: *mut LPVOID,
) -> HRESULT {
    if out_interface.is_null() {
        return -2147467262;
    }

    if uuid_bytes_equal(uuid, IUNKNOWN_UUID)
        || uuid_bytes_equal(uuid, AUDIO_SERVER_PLUGIN_DRIVER_INTERFACE_UUID)
    {
        *out_interface = driver;
        add_ref(driver);
        0
    } else {
        *out_interface = ptr::null_mut();
        -2147467262
    }
}

unsafe extern "C" fn add_ref(_driver: *mut c_void) -> ULONG {
    REF_COUNT.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "C" fn release(_driver: *mut c_void) -> ULONG {
    REF_COUNT.fetch_sub(1, Ordering::Relaxed).saturating_sub(1)
}

unsafe extern "C" fn initialize(
    _driver: AudioServerPlugInDriverRef,
    _host: AudioServerPlugInHostRef,
) -> OSStatus {
    ensure_reader();
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn create_device(
    _driver: AudioServerPlugInDriverRef,
    _description: CFDictionaryRef,
    _client_info: *const AudioServerPlugInClientInfo,
    out_device_object_id: *mut AudioObjectID,
) -> OSStatus {
    if !out_device_object_id.is_null() {
        *out_device_object_id = DEVICE_ID;
    }
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn destroy_device(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn add_device_client(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _client_info: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn remove_device_client(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _client_info: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn perform_device_configuration_change(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _change_action: UInt64,
    _change_info: *mut c_void,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn abort_device_configuration_change(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _change_action: UInt64,
    _change_info: *mut c_void,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn has_property(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _client_process_id: pid_t,
    address: *const AudioObjectPropertyAddress,
) -> Boolean {
    if address.is_null() {
        return 0;
    }
    property_kind(object_id, (*address).mSelector).is_some() as Boolean
}

unsafe extern "C" fn is_property_settable(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _client_process_id: pid_t,
    address: *const AudioObjectPropertyAddress,
    out_is_settable: *mut Boolean,
) -> OSStatus {
    if address.is_null() || out_is_settable.is_null() {
        return kAudioHardwareBadObjectError as OSStatus;
    }
    if property_kind(object_id, (*address).mSelector).is_none() {
        return kAudioHardwareUnknownPropertyError as OSStatus;
    }
    *out_is_settable = 0;
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn get_property_data_size(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _client_process_id: pid_t,
    address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: UInt32,
    _qualifier_data: *const c_void,
    out_data_size: *mut UInt32,
) -> OSStatus {
    if address.is_null() || out_data_size.is_null() {
        return kAudioHardwareBadObjectError as OSStatus;
    }
    let Some(kind) = property_kind(object_id, (*address).mSelector) else {
        return kAudioHardwareUnknownPropertyError as OSStatus;
    };
    *out_data_size = property_size(kind, object_id, (*address).mScope);
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn get_property_data(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _client_process_id: pid_t,
    address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: UInt32,
    _qualifier_data: *const c_void,
    data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
) -> OSStatus {
    if address.is_null() || out_data_size.is_null() || out_data.is_null() {
        return kAudioHardwareBadObjectError as OSStatus;
    }
    let address = *address;
    let Some(kind) = property_kind(object_id, address.mSelector) else {
        return kAudioHardwareUnknownPropertyError as OSStatus;
    };

    match kind {
        PropertyKind::Class => write_value(data_size, out_data_size, out_data, class_id(object_id)),
        PropertyKind::BaseClass => {
            write_value(data_size, out_data_size, out_data, base_class_id(object_id))
        }
        PropertyKind::Owner => write_value(data_size, out_data_size, out_data, owner_id(object_id)),
        PropertyKind::Name => {
            write_cf_string(data_size, out_data_size, out_data, object_name(object_id))
        }
        PropertyKind::Manufacturer => {
            write_cf_string(data_size, out_data_size, out_data, MANUFACTURER)
        }
        PropertyKind::OwnedObjects => {
            write_owned_objects(data_size, out_data_size, out_data, object_id)
        }
        PropertyKind::BundleId => write_cf_string(data_size, out_data_size, out_data, DRIVER_UID),
        PropertyKind::ResourceBundle => {
            write_cf_string(data_size, out_data_size, out_data, "Resources")
        }
        PropertyKind::DeviceList => write_slice(data_size, out_data_size, out_data, &[DEVICE_ID]),
        PropertyKind::EmptyObjectList
        | PropertyKind::ControlList
        | PropertyKind::CustomPropertyInfoList => {
            *out_data_size = 0;
            kAudioHardwareNoError as OSStatus
        }
        PropertyKind::TranslateDeviceUid => {
            write_value(data_size, out_data_size, out_data, DEVICE_ID)
        }
        PropertyKind::DeviceUid => write_cf_string(data_size, out_data_size, out_data, DEVICE_UID),
        PropertyKind::ModelUid => write_cf_string(data_size, out_data_size, out_data, MODEL_UID),
        PropertyKind::TransportType => write_value(
            data_size,
            out_data_size,
            out_data,
            kAudioDeviceTransportTypeVirtual,
        ),
        PropertyKind::Alive => write_value(data_size, out_data_size, out_data, 1u32),
        PropertyKind::Running => write_value(
            data_size,
            out_data_size,
            out_data,
            IO_RUNNING.load(Ordering::Acquire) as u32,
        ),
        PropertyKind::DefaultDevice => write_value(data_size, out_data_size, out_data, 1u32),
        PropertyKind::DefaultSystemDevice => write_value(data_size, out_data_size, out_data, 0u32),
        PropertyKind::RelatedDevices => {
            write_slice(data_size, out_data_size, out_data, &[DEVICE_ID])
        }
        PropertyKind::ClockDomain => write_value(data_size, out_data_size, out_data, 0u32),
        PropertyKind::Hidden => write_value(data_size, out_data_size, out_data, 0u32),
        PropertyKind::HogMode => write_value(data_size, out_data_size, out_data, -1i32),
        PropertyKind::SupportsMixing => write_value(data_size, out_data_size, out_data, 1u32),
        PropertyKind::Latency | PropertyKind::SafetyOffset => {
            write_value(data_size, out_data_size, out_data, 0u32)
        }
        PropertyKind::SampleRate | PropertyKind::ActualSampleRate => {
            write_value(data_size, out_data_size, out_data, SAMPLE_RATE as Float64)
        }
        PropertyKind::AvailableSampleRates => write_value(
            data_size,
            out_data_size,
            out_data,
            AudioValueRange {
                mMinimum: SAMPLE_RATE as Float64,
                mMaximum: SAMPLE_RATE as Float64,
            },
        ),
        PropertyKind::Streams => write_streams(data_size, out_data_size, out_data, address.mScope),
        PropertyKind::StreamConfiguration => {
            write_stream_configuration(data_size, out_data_size, out_data, address.mScope)
        }
        PropertyKind::BufferFrameSize => {
            write_value(data_size, out_data_size, out_data, BUFFER_FRAME_SIZE)
        }
        PropertyKind::BufferFrameSizeRange => write_value(
            data_size,
            out_data_size,
            out_data,
            AudioValueRange {
                mMinimum: 64.0,
                mMaximum: 4096.0,
            },
        ),
        PropertyKind::ZeroTimeStampPeriod => {
            write_value(data_size, out_data_size, out_data, BUFFER_FRAME_SIZE)
        }
        PropertyKind::StreamActive => write_value(data_size, out_data_size, out_data, 1u32),
        PropertyKind::StreamDirection => write_value(data_size, out_data_size, out_data, 1u32),
        PropertyKind::StreamTerminalType => write_value(
            data_size,
            out_data_size,
            out_data,
            kAudioStreamTerminalTypeMicrophone,
        ),
        PropertyKind::StreamStartingChannel => {
            write_value(data_size, out_data_size, out_data, 1u32)
        }
        PropertyKind::StreamFormat | PropertyKind::StreamPhysicalFormat => {
            write_value(data_size, out_data_size, out_data, stream_format())
        }
        PropertyKind::StreamAvailableFormats => write_value(
            data_size,
            out_data_size,
            out_data,
            stream_ranged_description(),
        ),
    }
}

unsafe extern "C" fn set_property_data(
    _driver: AudioServerPlugInDriverRef,
    object_id: AudioObjectID,
    _client_process_id: pid_t,
    address: *const AudioObjectPropertyAddress,
    _qualifier_data_size: UInt32,
    _qualifier_data: *const c_void,
    _data_size: UInt32,
    _data: *const c_void,
) -> OSStatus {
    if address.is_null() {
        return kAudioHardwareBadObjectError as OSStatus;
    }
    if property_kind(object_id, (*address).mSelector).is_none() {
        return kAudioHardwareUnknownPropertyError as OSStatus;
    }
    kAudioHardwareIllegalOperationError as OSStatus
}

unsafe extern "C" fn start_io(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
) -> OSStatus {
    ensure_reader();
    IO_RUNNING.store(true, Ordering::Release);
    ZERO_TIME_SEED.fetch_add(1, Ordering::AcqRel);
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn stop_io(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
) -> OSStatus {
    IO_RUNNING.store(false, Ordering::Release);
    ZERO_TIME_SEED.fetch_add(1, Ordering::AcqRel);
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn get_zero_time_stamp(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
    out_sample_time: *mut Float64,
    out_host_time: *mut UInt64,
    out_seed: *mut UInt64,
) -> OSStatus {
    if out_sample_time.is_null() || out_host_time.is_null() || out_seed.is_null() {
        return kAudioHardwareBadObjectError as OSStatus;
    }
    *out_sample_time = SAMPLE_TIME.load(Ordering::Acquire) as Float64;
    *out_host_time = AudioGetCurrentHostTime();
    *out_seed = ZERO_TIME_SEED.load(Ordering::Acquire);
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn will_do_io_operation(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
    operation_id: UInt32,
    out_will_do: *mut Boolean,
    out_will_do_in_place: *mut Boolean,
) -> OSStatus {
    if out_will_do.is_null() || out_will_do_in_place.is_null() {
        return kAudioHardwareBadObjectError as OSStatus;
    }
    *out_will_do = (operation_id == kAudioServerPlugInIOOperationReadInput) as Boolean;
    *out_will_do_in_place = 0;
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn begin_io_operation(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
    _operation_id: UInt32,
    _io_buffer_frame_size: UInt32,
    _io_cycle_info: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn do_io_operation(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    stream_object_id: AudioObjectID,
    _client_id: UInt32,
    operation_id: UInt32,
    io_buffer_frame_size: UInt32,
    _io_cycle_info: *const AudioServerPlugInIOCycleInfo,
    main_buffer: *mut c_void,
    _secondary_buffer: *mut c_void,
) -> OSStatus {
    if operation_id != kAudioServerPlugInIOOperationReadInput || stream_object_id != STREAM_ID {
        return kAudioHardwareNoError as OSStatus;
    }

    if main_buffer.is_null() {
        return kAudioHardwareBadObjectError as OSStatus;
    }

    let out =
        std::slice::from_raw_parts_mut(main_buffer.cast::<f32>(), io_buffer_frame_size as usize);
    let reader = READER.load(Ordering::Acquire);
    if reader.is_null() {
        out.fill(0.0);
    } else {
        (*reader).read_f32(out);
    }
    SAMPLE_TIME.fetch_add(io_buffer_frame_size as u64, Ordering::AcqRel);
    kAudioHardwareNoError as OSStatus
}

unsafe extern "C" fn end_io_operation(
    _driver: AudioServerPlugInDriverRef,
    _device_object_id: AudioObjectID,
    _client_id: UInt32,
    _operation_id: UInt32,
    _io_buffer_frame_size: UInt32,
    _io_cycle_info: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    kAudioHardwareNoError as OSStatus
}

#[derive(Clone, Copy)]
enum PropertyKind {
    Class,
    BaseClass,
    Owner,
    Name,
    Manufacturer,
    OwnedObjects,
    BundleId,
    ResourceBundle,
    DeviceList,
    EmptyObjectList,
    ControlList,
    TranslateDeviceUid,
    CustomPropertyInfoList,
    DeviceUid,
    ModelUid,
    TransportType,
    Alive,
    Running,
    DefaultDevice,
    DefaultSystemDevice,
    RelatedDevices,
    ClockDomain,
    Hidden,
    HogMode,
    SupportsMixing,
    Latency,
    SafetyOffset,
    SampleRate,
    ActualSampleRate,
    AvailableSampleRates,
    Streams,
    StreamConfiguration,
    BufferFrameSize,
    BufferFrameSizeRange,
    ZeroTimeStampPeriod,
    StreamActive,
    StreamDirection,
    StreamTerminalType,
    StreamStartingChannel,
    StreamFormat,
    StreamPhysicalFormat,
    StreamAvailableFormats,
}

fn property_kind(
    object_id: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Option<PropertyKind> {
    match object_id {
        kAudioObjectPlugInObject => match selector {
            kAudioObjectPropertyClass => Some(PropertyKind::Class),
            kAudioObjectPropertyBaseClass => Some(PropertyKind::BaseClass),
            kAudioObjectPropertyOwner => Some(PropertyKind::Owner),
            kAudioObjectPropertyName => Some(PropertyKind::Name),
            kAudioObjectPropertyManufacturer => Some(PropertyKind::Manufacturer),
            kAudioObjectPropertyOwnedObjects => Some(PropertyKind::OwnedObjects),
            kAudioObjectPropertyCustomPropertyInfoList => {
                Some(PropertyKind::CustomPropertyInfoList)
            }
            kAudioPlugInPropertyBundleID => Some(PropertyKind::BundleId),
            kAudioPlugInPropertyResourceBundle => Some(PropertyKind::ResourceBundle),
            kAudioPlugInPropertyDeviceList => Some(PropertyKind::DeviceList),
            kAudioPlugInPropertyTranslateUIDToDevice => Some(PropertyKind::TranslateDeviceUid),
            kAudioPlugInPropertyBoxList | kAudioPlugInPropertyClockDeviceList => {
                Some(PropertyKind::EmptyObjectList)
            }
            _ => None,
        },
        DEVICE_ID => match selector {
            kAudioObjectPropertyClass => Some(PropertyKind::Class),
            kAudioObjectPropertyBaseClass => Some(PropertyKind::BaseClass),
            kAudioObjectPropertyOwner => Some(PropertyKind::Owner),
            kAudioObjectPropertyName => Some(PropertyKind::Name),
            kAudioObjectPropertyManufacturer => Some(PropertyKind::Manufacturer),
            kAudioObjectPropertyOwnedObjects => Some(PropertyKind::OwnedObjects),
            kAudioObjectPropertyCustomPropertyInfoList => {
                Some(PropertyKind::CustomPropertyInfoList)
            }
            kAudioObjectPropertyControlList => Some(PropertyKind::ControlList),
            kAudioDevicePropertyDeviceUID => Some(PropertyKind::DeviceUid),
            kAudioDevicePropertyModelUID => Some(PropertyKind::ModelUid),
            kAudioDevicePropertyTransportType => Some(PropertyKind::TransportType),
            kAudioDevicePropertyDeviceIsAlive => Some(PropertyKind::Alive),
            kAudioDevicePropertyDeviceIsRunning | kAudioDevicePropertyDeviceIsRunningSomewhere => {
                Some(PropertyKind::Running)
            }
            kAudioDevicePropertyDeviceCanBeDefaultDevice => Some(PropertyKind::DefaultDevice),
            kAudioDevicePropertyDeviceCanBeDefaultSystemDevice => {
                Some(PropertyKind::DefaultSystemDevice)
            }
            kAudioDevicePropertyRelatedDevices => Some(PropertyKind::RelatedDevices),
            kAudioDevicePropertyClockDomain => Some(PropertyKind::ClockDomain),
            kAudioDevicePropertyIsHidden => Some(PropertyKind::Hidden),
            kAudioDevicePropertyHogMode => Some(PropertyKind::HogMode),
            kAudioDevicePropertySupportsMixing => Some(PropertyKind::SupportsMixing),
            kAudioDevicePropertyLatency => Some(PropertyKind::Latency),
            kAudioDevicePropertySafetyOffset => Some(PropertyKind::SafetyOffset),
            kAudioDevicePropertyNominalSampleRate => Some(PropertyKind::SampleRate),
            kAudioDevicePropertyActualSampleRate => Some(PropertyKind::ActualSampleRate),
            kAudioDevicePropertyAvailableNominalSampleRates => {
                Some(PropertyKind::AvailableSampleRates)
            }
            kAudioDevicePropertyStreams => Some(PropertyKind::Streams),
            kAudioDevicePropertyStreamConfiguration => Some(PropertyKind::StreamConfiguration),
            kAudioDevicePropertyBufferFrameSize => Some(PropertyKind::BufferFrameSize),
            kAudioDevicePropertyBufferFrameSizeRange => Some(PropertyKind::BufferFrameSizeRange),
            kAudioDevicePropertyZeroTimeStampPeriod => Some(PropertyKind::ZeroTimeStampPeriod),
            _ => None,
        },
        STREAM_ID => match selector {
            kAudioObjectPropertyClass => Some(PropertyKind::Class),
            kAudioObjectPropertyBaseClass => Some(PropertyKind::BaseClass),
            kAudioObjectPropertyOwner => Some(PropertyKind::Owner),
            kAudioObjectPropertyName => Some(PropertyKind::Name),
            kAudioObjectPropertyCustomPropertyInfoList => {
                Some(PropertyKind::CustomPropertyInfoList)
            }
            kAudioStreamPropertyIsActive => Some(PropertyKind::StreamActive),
            kAudioStreamPropertyDirection => Some(PropertyKind::StreamDirection),
            kAudioStreamPropertyTerminalType => Some(PropertyKind::StreamTerminalType),
            kAudioStreamPropertyStartingChannel => Some(PropertyKind::StreamStartingChannel),
            kAudioStreamPropertyLatency => Some(PropertyKind::Latency),
            kAudioStreamPropertyVirtualFormat => Some(PropertyKind::StreamFormat),
            kAudioStreamPropertyPhysicalFormat => Some(PropertyKind::StreamPhysicalFormat),
            kAudioStreamPropertyAvailableVirtualFormats
            | kAudioStreamPropertyAvailablePhysicalFormats => {
                Some(PropertyKind::StreamAvailableFormats)
            }
            _ => None,
        },
        _ => None,
    }
}

fn property_size(
    kind: PropertyKind,
    object_id: AudioObjectID,
    scope: AudioObjectPropertyScope,
) -> UInt32 {
    match kind {
        PropertyKind::Name
        | PropertyKind::Manufacturer
        | PropertyKind::BundleId
        | PropertyKind::ResourceBundle
        | PropertyKind::DeviceUid
        | PropertyKind::ModelUid => size_of::<CFStringRef>() as u32,
        PropertyKind::SampleRate | PropertyKind::ActualSampleRate => size_of::<Float64>() as u32,
        PropertyKind::AvailableSampleRates | PropertyKind::BufferFrameSizeRange => {
            size_of::<AudioValueRange>() as u32
        }
        PropertyKind::StreamAvailableFormats => size_of::<AudioStreamRangedDescription>() as u32,
        PropertyKind::StreamConfiguration => size_of::<AudioBufferList>() as u32,
        PropertyKind::RelatedDevices => size_of::<AudioObjectID>() as u32,
        PropertyKind::DeviceList => size_of::<AudioObjectID>() as u32,
        PropertyKind::EmptyObjectList
        | PropertyKind::ControlList
        | PropertyKind::CustomPropertyInfoList => 0,
        PropertyKind::TranslateDeviceUid => size_of::<AudioObjectID>() as u32,
        PropertyKind::OwnedObjects => {
            if object_id == kAudioObjectPlugInObject || object_id == DEVICE_ID {
                size_of::<AudioObjectID>() as u32
            } else {
                0
            }
        }
        PropertyKind::Streams => {
            if scope == kAudioObjectPropertyScopeInput || scope == kAudioObjectPropertyScopeGlobal {
                size_of::<AudioObjectID>() as u32
            } else {
                0
            }
        }
        PropertyKind::StreamFormat | PropertyKind::StreamPhysicalFormat => {
            size_of::<AudioStreamBasicDescription>() as u32
        }
        _ => size_of::<u32>() as u32,
    }
}

fn class_id(object_id: AudioObjectID) -> AudioClassID {
    match object_id {
        kAudioObjectPlugInObject => kAudioPlugInClassID,
        DEVICE_ID => kAudioDeviceClassID,
        STREAM_ID => kAudioStreamClassID,
        _ => kAudioObjectClassID,
    }
}

fn base_class_id(object_id: AudioObjectID) -> AudioClassID {
    match object_id {
        kAudioObjectPlugInObject => kAudioObjectClassID,
        DEVICE_ID => kAudioObjectClassID,
        STREAM_ID => kAudioObjectClassID,
        _ => kAudioObjectClassID,
    }
}

fn owner_id(object_id: AudioObjectID) -> AudioObjectID {
    match object_id {
        kAudioObjectPlugInObject => 0,
        DEVICE_ID => kAudioObjectPlugInObject,
        STREAM_ID => DEVICE_ID,
        _ => 0,
    }
}

fn object_name(object_id: AudioObjectID) -> &'static str {
    match object_id {
        kAudioObjectPlugInObject => DRIVER_UID,
        DEVICE_ID => DEVICE_NAME,
        STREAM_ID => "m5mic input",
        _ => DEVICE_NAME,
    }
}

fn stream_format() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: SAMPLE_RATE as Float64,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagsNativeEndian,
        mBytesPerPacket: CHANNELS * size_of::<f32>() as u32,
        mFramesPerPacket: 1,
        mBytesPerFrame: CHANNELS * size_of::<f32>() as u32,
        mChannelsPerFrame: CHANNELS,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

fn stream_ranged_description() -> AudioStreamRangedDescription {
    AudioStreamRangedDescription {
        mFormat: stream_format(),
        mSampleRateRange: AudioValueRange {
            mMinimum: SAMPLE_RATE as Float64,
            mMaximum: SAMPLE_RATE as Float64,
        },
    }
}

unsafe fn write_value<T: Copy>(
    data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
    value: T,
) -> OSStatus {
    if data_size < size_of::<T>() as u32 {
        return kAudioHardwareBadPropertySizeError as OSStatus;
    }
    ptr::write(out_data.cast::<T>(), value);
    *out_data_size = size_of::<T>() as u32;
    kAudioHardwareNoError as OSStatus
}

unsafe fn write_cf_string(
    data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
    value: &str,
) -> OSStatus {
    let c_value = match CString::new(value) {
        Ok(value) => value,
        Err(_) => return kAudioHardwareUnspecifiedError as OSStatus,
    };
    let string =
        CFStringCreateWithCString(kCFAllocatorDefault, c_value.as_ptr(), kCFStringEncodingUTF8);
    write_value(data_size, out_data_size, out_data, string)
}

unsafe fn write_owned_objects(
    data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
    object_id: AudioObjectID,
) -> OSStatus {
    let objects = match object_id {
        kAudioObjectPlugInObject => [DEVICE_ID],
        DEVICE_ID => [STREAM_ID],
        _ => [0],
    };
    if object_id != kAudioObjectPlugInObject && object_id != DEVICE_ID {
        *out_data_size = 0;
        return kAudioHardwareNoError as OSStatus;
    }
    write_slice(data_size, out_data_size, out_data, &objects)
}

unsafe fn write_streams(
    data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
    scope: AudioObjectPropertyScope,
) -> OSStatus {
    if scope == kAudioObjectPropertyScopeOutput {
        *out_data_size = 0;
        return kAudioHardwareNoError as OSStatus;
    }
    write_slice(data_size, out_data_size, out_data, &[STREAM_ID])
}

unsafe fn write_stream_configuration(
    data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
    scope: AudioObjectPropertyScope,
) -> OSStatus {
    if data_size < size_of::<AudioBufferList>() as u32 {
        return kAudioHardwareBadPropertySizeError as OSStatus;
    }
    let list = out_data.cast::<AudioBufferList>();
    (*list).mNumberBuffers = if scope == kAudioObjectPropertyScopeOutput {
        0
    } else {
        1
    };
    (*list).mBuffers[0].mNumberChannels = CHANNELS;
    (*list).mBuffers[0].mDataByteSize = 0;
    (*list).mBuffers[0].mData = ptr::null_mut();
    *out_data_size = size_of::<AudioBufferList>() as u32;
    kAudioHardwareNoError as OSStatus
}

unsafe fn write_slice<T: Copy>(
    data_size: UInt32,
    out_data_size: *mut UInt32,
    out_data: *mut c_void,
    values: &[T],
) -> OSStatus {
    let byte_size = std::mem::size_of_val(values) as u32;
    if data_size < byte_size {
        return kAudioHardwareBadPropertySizeError as OSStatus;
    }
    ptr::copy_nonoverlapping(values.as_ptr(), out_data.cast::<T>(), values.len());
    *out_data_size = byte_size;
    kAudioHardwareNoError as OSStatus
}

fn ensure_reader() {
    if !READER.load(Ordering::Acquire).is_null() {
        return;
    }
    let Ok(reader) = VirtualMicReader::open_default() else {
        return;
    };
    let boxed = Box::into_raw(Box::new(reader));
    if READER
        .compare_exchange(ptr::null_mut(), boxed, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        unsafe {
            drop(Box::from_raw(boxed));
        }
    }
}

unsafe fn cf_uuid_equals(uuid: CFUUIDRef, expected: [u8; 16]) -> bool {
    let expected = CFUUIDGetConstantUUIDWithBytes(
        ptr::null(),
        expected[0],
        expected[1],
        expected[2],
        expected[3],
        expected[4],
        expected[5],
        expected[6],
        expected[7],
        expected[8],
        expected[9],
        expected[10],
        expected[11],
        expected[12],
        expected[13],
        expected[14],
        expected[15],
    );
    CFEqual(uuid.cast(), expected.cast()) != 0
}

fn uuid_bytes_equal(uuid: REFIID, expected: [u8; 16]) -> bool {
    [
        uuid.byte0,
        uuid.byte1,
        uuid.byte2,
        uuid.byte3,
        uuid.byte4,
        uuid.byte5,
        uuid.byte6,
        uuid.byte7,
        uuid.byte8,
        uuid.byte9,
        uuid.byte10,
        uuid.byte11,
        uuid.byte12,
        uuid.byte13,
        uuid.byte14,
        uuid.byte15,
    ] == expected
}
