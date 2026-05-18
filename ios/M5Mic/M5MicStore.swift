import Foundation
import Observation

@MainActor
@Observable
final class M5MicStore {
    var selectedTransport: M5MicTransport = .usb {
        didSet {
            guard selectedTransport != oldValue else { return }
            if deviceRecordingRequested {
                sendRecordingCommand(.stop)
                deviceRecordingRequested = false
                deviceRecordingStatusText = "Stopped"
            }
            activateSelectedTransport()
        }
    }
    var phase: ReceiverPhase = .usbMode
    var monitorEnabled = true {
        didSet {
            if !monitorEnabled {
                audioMonitor.stop()
            }
        }
    }
    var recordingEnabled = false
    var streamID: UInt32?
    var inputLevel = 0.0
    var frameCount = 0
    var sampleCount = 0
    var wifiDiscoveryReplyCount = 0
    var wifiHandshakeCount = 0
    var deviceRecordingRequested = false
    var deviceRecordingStatusText = "Stopped"
    var recordingStatusText = "Off"
    var recordingFileName = "none"
    var recordingDestinationName = "Not selected"
    var recordingDestinationConfigured = false
    var savedRecordingCount = 0
    var filesSavedCount = 0
    var recordingSampleCount = 0
    var recordingError: String?
    var lastError: String?
    var receiverURL = ""

    @ObservationIgnored private let bluetooth = BluetoothReceiver()
    @ObservationIgnored private let bluetoothModeSender = BluetoothModeCommandSender()
    @ObservationIgnored private let wifi = WiFiReceiver()
    @ObservationIgnored private let modeSender = ModeCommandSender()
    @ObservationIgnored private let audioMonitor = AudioMonitor()
    @ObservationIgnored private let audioRecorder = AudioRecorder()
    @ObservationIgnored private let recordingDestination = RecordingDestination()
    @ObservationIgnored private var decoder: AudioDecoder?

    var isRunning: Bool {
        switch phase {
        case .idle, .usbMode, .failed:
            false
        default:
            true
        }
    }

    var recordingFolderURL: URL? {
        recordingDestination.folderURL
    }

    var streamIDText: String {
        guard let streamID else { return "none" }
        return String(format: "%08x", streamID)
    }

    init() {
        decoder = try? AudioDecoder()
        receiverURL = wifi.receiverURL
        syncRecordingState()
        syncRecordingDestinationState()
        reconcileSavedRecordingsToFiles()
        bluetooth.onPhase = { [weak self] phase in
            Task { @MainActor in
                guard self?.selectedTransport == .bluetooth else { return }
                self?.setPhase(phase)
            }
        }
        bluetooth.onFrame = { [weak self] frame in
            Task { @MainActor in
                guard self?.selectedTransport == .bluetooth else { return }
                self?.handleFrame(frame)
            }
        }
        wifi.onPhase = { [weak self] phase in
            Task { @MainActor in
                guard self?.selectedTransport == .wifi else { return }
                self?.setPhase(phase)
            }
        }
        wifi.onFrame = { [weak self] frame in
            Task { @MainActor in
                guard self?.selectedTransport == .wifi else { return }
                self?.handleFrame(frame)
            }
        }
        wifi.onDiscoveryReply = { [weak self] in
            Task { @MainActor in
                guard self?.selectedTransport == .wifi else { return }
                self?.wifiDiscoveryReplyCount += 1
            }
        }
        wifi.onHandshake = { [weak self] in
            Task { @MainActor in
                guard self?.selectedTransport == .wifi else { return }
                self?.wifiHandshakeCount += 1
            }
        }
    }

    func start() {
        activateSelectedTransport()
    }

    func stop() {
        if deviceRecordingRequested {
            sendRecordingCommand(.stop)
            deviceRecordingRequested = false
            deviceRecordingStatusText = "Stopped"
        }
        stopReceivers()
        phase = selectedTransport == .usb ? .usbMode : .idle
        resetStream()
    }

    func selectTransport(_ transport: M5MicTransport) {
        if deviceRecordingRequested, selectedTransport != transport {
            sendRecordingCommand(.stop)
            deviceRecordingRequested = false
            deviceRecordingStatusText = "Stopped"
        }
        selectedTransport = transport
    }

    func refreshReceiverURL() {
        receiverURL = wifi.receiverURL
    }

    func setRecordingEnabled(_ enabled: Bool) {
        guard enabled != recordingEnabled else { return }

        if enabled, !recordingDestination.isConfigured {
            recordingError = "Choose a Files folder before recording."
            syncRecordingState()
            return
        }

        recordingEnabled = enabled
        if recordingEnabled {
            syncRecordingState()
        } else {
            finishRecording()
        }
    }

    func selectRecordingFolder(_ url: URL) {
        do {
            try recordingDestination.selectFolder(url)
            recordingError = nil
            syncRecordingDestinationState()
            try copySavedRecordingsToFiles()
        } catch {
            recordingError = error.localizedDescription
            syncRecordingDestinationState()
        }
    }

    func clearRecordingError() {
        recordingError = nil
    }

    func setDeviceRecordingEnabled(_ enabled: Bool) {
        guard selectedTransport != .usb else {
            deviceRecordingRequested = false
            deviceRecordingStatusText = "Unavailable"
            return
        }
        guard enabled != deviceRecordingRequested || enabled else { return }

        deviceRecordingRequested = enabled
        if enabled {
            deviceRecordingStatusText = "Starting"
            if !isRunning {
                activateSelectedTransport()
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { [weak self] in
                    guard let self, self.deviceRecordingRequested else { return }
                    self.sendRecordingCommand(.start)
                }
            } else {
                sendRecordingCommand(.start)
            }
        } else {
            sendRecordingCommand(.stop)
            deviceRecordingStatusText = "Stopped"
        }
    }

    private func activateSelectedTransport() {
        let mode = selectedTransport.mode
        modeSender.send(mode)
        if selectedTransport != .bluetooth {
            bluetoothModeSender.send(mode)
        }

        stopReceivers()
        resetStream()
        receiverURL = wifi.receiverURL
        if selectedTransport != .usb, !deviceRecordingRequested {
            deviceRecordingStatusText = "Stopped"
        }

        switch selectedTransport {
        case .bluetooth:
            phase = .scanning
            bluetooth.start()
        case .wifi:
            phase = .waiting
            wifi.start()
        case .usb:
            phase = .usbMode
        }
    }

    private func stopReceivers() {
        bluetooth.stop()
        wifi.stop()
        audioMonitor.stop()
        finishRecording()
        decoder?.reset()
    }

    func sendMode(_ mode: M5MicTransportMode) {
        modeSender.send(mode)
        bluetooth.sendMode(mode)
    }

    private func sendRecordingCommand(_ command: M5MicRecordingCommand) {
        modeSender.send(command)
        if selectedTransport == .bluetooth {
            bluetooth.sendRecordingCommand(command)
        }
    }

    private func setPhase(_ phase: ReceiverPhase) {
        self.phase = phase
        if case let .failed(message) = phase {
            lastError = message
        }
    }

    private func handleFrame(_ frame: Data) {
        guard let decoder else {
            phase = .failed("Rust decoder unavailable")
            return
        }

        do {
            let decoded = try decoder.decode(frame)
            streamID = decoded.streamID
            inputLevel = Double(decoded.level) / 100.0

            switch decoded.event {
            case .started:
                if audioRecorder.isRecording {
                    finishRecording()
                }
                deviceRecordingRequested = true
                deviceRecordingStatusText = "Streaming"
                frameCount = 1
                sampleCount = decoded.samples.count
                phase = .receiving
            case .audio:
                if deviceRecordingRequested {
                    deviceRecordingStatusText = "Streaming"
                }
                frameCount += 1
                sampleCount += decoded.samples.count
                phase = .receiving
            case .ended:
                deviceRecordingRequested = false
                deviceRecordingStatusText = "Stopped"
                finishRecording()
                phase = .connected
                audioMonitor.stop()
                return
            }

            if recordingEnabled {
                try record(decoded)
            }

            playMonitorIfNeeded(decoded.samples)
        } catch {
            finishRecording()
            if recordingEnabled {
                recordingError = error.localizedDescription
            }
            lastError = error.localizedDescription
            phase = .failed(error.localizedDescription)
        }
    }

    private func record(_ decoded: DecodedAudioFrame) throws {
        if !audioRecorder.isRecording {
            try audioRecorder.start(
                transport: selectedTransport,
                streamID: decoded.streamID,
                sampleRate: decoded.sampleRate,
                channels: decoded.channels
            )
        }
        try audioRecorder.write(decoded.samples)
        syncRecordingState()
    }

    private func playMonitorIfNeeded(_ samples: [Float]) {
        guard monitorEnabled else { return }

        do {
            try audioMonitor.play(samples)
        } catch {
            monitorEnabled = false
            audioMonitor.stop()
            lastError = "Speaker monitor disabled: \(error.localizedDescription)"
        }
    }

    private func finishRecording() {
        if let fileURL = audioRecorder.stop() {
            copyRecordingToFiles(fileURL)
        }
        syncRecordingState()
    }

    private func syncRecordingState() {
        if audioRecorder.isRecording {
            recordingStatusText = "Recording"
        } else if recordingEnabled {
            recordingStatusText = "Ready"
        } else {
            recordingStatusText = "Off"
        }
        recordingFileName = audioRecorder.currentFileName ?? audioRecorder.lastFileName ?? "none"
        savedRecordingCount = audioRecorder.savedFileCount
        recordingSampleCount = audioRecorder.isRecording ? audioRecorder.currentSampleCount : audioRecorder.lastSampleCount
    }

    private func syncRecordingDestinationState() {
        recordingDestinationName = recordingDestination.folderName
        recordingDestinationConfigured = recordingDestination.isConfigured
    }

    private func copyRecordingToFiles(_ fileURL: URL) {
        guard recordingDestination.isConfigured else { return }

        do {
            if try recordingDestination.copyRecording(fileURL) {
                filesSavedCount += 1
            }
        } catch {
            recordingError = error.localizedDescription
        }
        syncRecordingDestinationState()
    }

    private func copySavedRecordingsToFiles() throws {
        guard recordingDestination.isConfigured else { return }

        var savedCount = 0
        for fileURL in try audioRecorder.savedRecordingURLs() {
            if try recordingDestination.copyRecording(fileURL) {
                savedCount += 1
            }
        }
        filesSavedCount = savedCount
        syncRecordingDestinationState()
    }

    private func reconcileSavedRecordingsToFiles() {
        guard recordingDestination.isConfigured else { return }

        do {
            try copySavedRecordingsToFiles()
        } catch {
            recordingError = error.localizedDescription
            syncRecordingDestinationState()
        }
    }

    private func resetStream() {
        streamID = nil
        inputLevel = 0
        frameCount = 0
        sampleCount = 0
        wifiDiscoveryReplyCount = 0
        wifiHandshakeCount = 0
        lastError = nil
        decoder?.reset()
    }
}
