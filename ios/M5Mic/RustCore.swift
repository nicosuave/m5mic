import Foundation

enum M5MicTransportMode: Sendable {
    case wifi
    case bluetooth
    case usb
}

enum M5MicRecordingCommand: Sendable {
    case start
    case stop
}

enum ReceiverPhase: Equatable, Sendable {
    case idle
    case usbMode
    case scanning
    case connecting
    case waiting
    case connected
    case receiving
    case failed(String)

    var title: String {
        switch self {
        case .idle:
            "Idle"
        case .usbMode:
            "USB Mode"
        case .scanning:
            "Scanning"
        case .connecting:
            "Connecting"
        case .waiting:
            "Waiting"
        case .connected:
            "Connected"
        case .receiving:
            "Receiving"
        case let .failed(message):
            message
        }
    }

    var systemImage: String {
        switch self {
        case .idle:
            "circle"
        case .usbMode:
            "cable.connector"
        case .scanning:
            "dot.radiowaves.left.and.right"
        case .connecting:
            "antenna.radiowaves.left.and.right"
        case .waiting:
            "clock"
        case .connected:
            "checkmark.circle"
        case .receiving:
            "record.circle"
        case .failed:
            "exclamationmark.triangle"
        }
    }
}

enum M5MicTransport: String, CaseIterable, Identifiable, Sendable {
    case wifi
    case bluetooth
    case usb

    var id: String { rawValue }

    var title: String {
        switch self {
        case .bluetooth:
            "Bluetooth"
        case .wifi:
            "Wi-Fi"
        case .usb:
            "USB"
        }
    }

    var systemImage: String {
        switch self {
        case .bluetooth:
            "dot.radiowaves.left.and.right"
        case .wifi:
            "wifi"
        case .usb:
            "cable.connector"
        }
    }

    var mode: M5MicTransportMode {
        switch self {
        case .bluetooth:
            .bluetooth
        case .wifi:
            .wifi
        case .usb:
            .usb
        }
    }
}

enum DecodeEvent: Sendable {
    case started
    case audio
    case ended
}

struct DecodedAudioFrame: Sendable {
    let event: DecodeEvent
    let samples: [Float]
    let streamID: UInt32
    let level: UInt8
    let sampleRate: UInt32
    let channels: UInt8
    let flags: UInt16
}

enum M5MicCoreError: LocalizedError {
    case allocationFailed
    case emptyFrame
    case decodeFailed(Int32)
    case reassemblyFailed(Int32)

    var errorDescription: String? {
        switch self {
        case .allocationFailed:
            "Rust core allocation failed"
        case .emptyFrame:
            "Empty audio frame"
        case let .decodeFailed(status):
            "Audio decode failed (\(status))"
        case let .reassemblyFailed(status):
            "Bluetooth frame reassembly failed (\(status))"
        }
    }
}

enum RustConstants {
    static var discoveryPort: UInt16 { m5mic_discovery_port() }
    static var controlPort: UInt16 { m5mic_control_port() }
    static var webSocketPort: UInt16 { m5mic_ws_port() }
    static var webSocketPath: String { string(m5mic_ws_path()) }
    static var bonjourServiceType: String { string(m5mic_bonjour_service_type()) }
    static var discoveryResponsePrefix: String { string(m5mic_discovery_response_prefix()) }
    static var bluetoothServiceUUID: String { string(m5mic_ble_service_uuid()) }
    static var bluetoothAudioCharacteristicUUID: String { string(m5mic_ble_audio_characteristic_uuid()) }
    static var bluetoothControlCharacteristicUUID: String { string(m5mic_ble_control_characteristic_uuid()) }
    static var bluetoothStatusCharacteristicUUID: String { string(m5mic_ble_status_characteristic_uuid()) }
    static var defaultFrameCapacity: Int { max(Int(m5mic_default_frame_capacity()), 512) }
    static var defaultOutputSampleCapacity: Int { max(Int(m5mic_default_output_sample_capacity()), 1_920) }

    static var discoveryRequest: Data {
        var length = 0
        guard let pointer = m5mic_discovery_request(&length) else { return Data() }
        return Data(bytes: pointer, count: length)
    }

    static func controlPayloads(for mode: M5MicTransportMode) -> [Data] {
        let legacyPayload = legacyControlPayload(for: mode)
        return prioritizedPayloads(for: legacyPayload)
    }

    static func controlPayload(for mode: M5MicTransportMode) -> Data {
        controlPayloads(for: mode).last ?? Data()
    }

    static func controlPayloads(for command: M5MicRecordingCommand) -> [Data] {
        let legacyPayload = legacyControlPayload(for: command)
        return prioritizedPayloads(for: legacyPayload)
    }

    static func controlPayload(for command: M5MicRecordingCommand) -> Data {
        controlPayloads(for: command).last ?? Data()
    }

    private static func prioritizedPayloads(for legacyPayload: Data) -> [Data] {
        guard !legacyPayload.isEmpty else { return [] }

        var priorityPayload = legacyPayload
        priorityPayload.append(contentsOf: " source=ios priority=100".utf8)
        return [legacyPayload, priorityPayload]
    }

    private static func legacyControlPayload(for mode: M5MicTransportMode) -> Data {
        var length = 0
        let pointer: UnsafePointer<UInt8>?
        switch mode {
        case .wifi:
            pointer = m5mic_control_mode_wifi(&length)
        case .bluetooth:
            pointer = m5mic_control_mode_ble(&length)
        case .usb:
            pointer = m5mic_control_mode_usb(&length)
        }
        guard let pointer else { return Data() }
        return Data(bytes: pointer, count: length)
    }

    private static func legacyControlPayload(for command: M5MicRecordingCommand) -> Data {
        var length = 0
        let pointer: UnsafePointer<UInt8>?
        switch command {
        case .start:
            pointer = m5mic_control_record_start(&length)
        case .stop:
            pointer = m5mic_control_record_stop(&length)
        }
        guard let pointer else { return Data() }
        return Data(bytes: pointer, count: length)
    }

    private static func string(_ pointer: UnsafePointer<CChar>?) -> String {
        guard let pointer else { return "" }
        return String(cString: pointer)
    }
}

final class AudioDecoder {
    private var decoder: OpaquePointer?

    init() throws {
        guard let decoder = m5mic_decoder_new() else {
            throw M5MicCoreError.allocationFailed
        }
        self.decoder = decoder
    }

    deinit {
        if let decoder {
            m5mic_decoder_free(decoder)
        }
    }

    func reset() {
        if let decoder {
            m5mic_decoder_reset(decoder)
        }
    }

    func decode(_ frame: Data) throws -> DecodedAudioFrame {
        guard !frame.isEmpty else { throw M5MicCoreError.emptyFrame }
        guard let decoder else { throw M5MicCoreError.allocationFailed }

        var samples = [Float](repeating: 0, count: RustConstants.defaultOutputSampleCapacity)
        var sampleCount = 0
        var streamID: UInt32 = 0
        var level: UInt8 = 0
        var sampleRate: UInt32 = 0
        var channels: UInt8 = 0
        var flags: UInt16 = 0

        let status = frame.withUnsafeBytes { frameBuffer in
            samples.withUnsafeMutableBufferPointer { sampleBuffer in
                m5mic_decode_frame(
                    decoder,
                    frameBuffer.bindMemory(to: UInt8.self).baseAddress,
                    frame.count,
                    sampleBuffer.baseAddress,
                    sampleBuffer.count,
                    &sampleCount,
                    &streamID,
                    &level,
                    &sampleRate,
                    &channels,
                    &flags
                )
            }
        }

        let event: DecodeEvent
        switch status {
        case 2:
            event = .started
        case 3:
            event = .audio
        case 4:
            event = .ended
        default:
            throw M5MicCoreError.decodeFailed(status)
        }

        return DecodedAudioFrame(
            event: event,
            samples: Array(samples.prefix(sampleCount)),
            streamID: streamID,
            level: level,
            sampleRate: sampleRate,
            channels: channels,
            flags: flags
        )
    }
}

final class BleFrameReassembler {
    private var reassembler: OpaquePointer?

    init() throws {
        guard let reassembler = m5mic_ble_reassembler_new() else {
            throw M5MicCoreError.allocationFailed
        }
        self.reassembler = reassembler
    }

    deinit {
        if let reassembler {
            m5mic_ble_reassembler_free(reassembler)
        }
    }

    func reset() {
        if let reassembler {
            m5mic_ble_reassembler_reset(reassembler)
        }
    }

    func push(_ fragment: Data) throws -> Data? {
        guard let reassembler else { throw M5MicCoreError.allocationFailed }
        var frame = [UInt8](repeating: 0, count: RustConstants.defaultFrameCapacity)
        var frameLength = 0

        let status = fragment.withUnsafeBytes { fragmentBuffer in
            frame.withUnsafeMutableBufferPointer { frameBuffer in
                m5mic_ble_reassembler_push(
                    reassembler,
                    fragmentBuffer.bindMemory(to: UInt8.self).baseAddress,
                    fragment.count,
                    frameBuffer.baseAddress,
                    frameBuffer.count,
                    &frameLength
                )
            }
        }

        switch status {
        case 0:
            return Data(frame.prefix(frameLength))
        case 1:
            return nil
        default:
            throw M5MicCoreError.reassemblyFailed(status)
        }
    }
}
