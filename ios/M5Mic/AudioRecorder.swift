import AVFoundation
import Foundation

@MainActor
final class AudioRecorder {
    private let fileManager = FileManager.default
    private var file: AVAudioFile?
    private var format: AVAudioFormat?

    private(set) var currentFileURL: URL?
    private(set) var lastFileURL: URL?
    private(set) var currentFileName: String?
    private(set) var lastFileName: String?
    private(set) var savedFileCount = 0
    private(set) var currentSampleCount = 0
    private(set) var lastSampleCount = 0

    var isRecording: Bool {
        file != nil
    }

    func start(
        transport: M5MicTransport,
        streamID: UInt32,
        sampleRate: UInt32,
        channels: UInt8,
        startedAt: Date = Date()
    ) throws {
        stop()

        let channelCount = max(AVAudioChannelCount(channels), 1)
        let effectiveSampleRate = sampleRate == 0 ? 48_000 : sampleRate
        guard let format = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: Double(effectiveSampleRate),
            channels: channelCount,
            interleaved: false
        ) else {
            throw AudioRecorderError.formatUnavailable
        }

        let url = try nextRecordingURL(
            transport: transport,
            streamID: streamID,
            startedAt: startedAt
        )
        do {
            file = try AVAudioFile(
                forWriting: url,
                settings: format.settings,
                commonFormat: .pcmFormatFloat32,
                interleaved: false
            )
        } catch {
            throw AudioRecorderError.operationFailed("Create recording file \(url.lastPathComponent)", error)
        }
        self.format = format
        currentFileURL = url
        currentFileName = url.lastPathComponent
        currentSampleCount = 0
    }

    func write(_ samples: [Float]) throws {
        guard !samples.isEmpty, let file, let format else { return }

        let channelCount = Int(format.channelCount)
        let frameCount = samples.count / channelCount
        guard frameCount > 0 else { return }
        guard let buffer = AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: AVAudioFrameCount(frameCount)
        ) else {
            throw AudioRecorderError.bufferUnavailable
        }

        buffer.frameLength = AVAudioFrameCount(frameCount)
        if let channelData = buffer.floatChannelData {
            if channelCount == 1 {
                samples.withUnsafeBufferPointer { source in
                    if let baseAddress = source.baseAddress {
                        channelData[0].update(from: baseAddress, count: frameCount)
                    }
                }
            } else {
                for channelIndex in 0..<channelCount {
                    for frameIndex in 0..<frameCount {
                        channelData[channelIndex][frameIndex] = samples[frameIndex * channelCount + channelIndex]
                    }
                }
            }
        }

        do {
            try file.write(from: buffer)
        } catch {
            throw AudioRecorderError.operationFailed("Write recording audio", error)
        }
        currentSampleCount += frameCount
    }

    @discardableResult
    func stop() -> URL? {
        guard file != nil else { return nil }
        file = nil
        format = nil
        lastFileURL = currentFileURL
        lastFileName = currentFileName
        currentFileURL = nil
        currentFileName = nil
        savedFileCount += 1
        lastSampleCount = currentSampleCount
        currentSampleCount = 0
        return lastFileURL
    }

    func savedRecordingURLs() throws -> [URL] {
        let directory = try recordingsDirectory()
        let urls = try fileManager.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: [.creationDateKey],
            options: [.skipsHiddenFiles]
        )
        return urls
            .filter { url in
                let fileExtension = url.pathExtension
                return fileExtension.localizedCaseInsensitiveCompare("caf") == .orderedSame
                    || fileExtension.localizedCaseInsensitiveCompare("wav") == .orderedSame
            }
            .sorted { lhs, rhs in
                let lhsDate = (try? lhs.resourceValues(forKeys: [.creationDateKey]).creationDate) ?? .distantPast
                let rhsDate = (try? rhs.resourceValues(forKeys: [.creationDateKey]).creationDate) ?? .distantPast
                return lhsDate < rhsDate
            }
    }

    static func recordingsDirectory(fileManager: FileManager = .default) throws -> URL {
        guard let documentsURL = fileManager.urls(for: .documentDirectory, in: .userDomainMask).first else {
            throw AudioRecorderError.documentsDirectoryUnavailable
        }
        let recordingsURL = documentsURL.appendingPathComponent("Recordings", isDirectory: true)
        do {
            try fileManager.createDirectory(at: recordingsURL, withIntermediateDirectories: true)
        } catch {
            throw AudioRecorderError.operationFailed("Create Recordings folder", error)
        }
        return recordingsURL
    }

    private func nextRecordingURL(
        transport: M5MicTransport,
        streamID: UInt32,
        startedAt: Date
    ) throws -> URL {
        let directory = try recordingsDirectory()
        let timestamp = timestampFormatter().string(from: startedAt)
        let streamText = String(format: "%08x", streamID)
        let baseName = "m5mic \(timestamp) \(transport.title) \(streamText)"

        var url = directory.appendingPathComponent("\(baseName).caf")
        var suffix = 2
        while fileManager.fileExists(atPath: url.path) {
            url = directory.appendingPathComponent("\(baseName) \(suffix).caf")
            suffix += 1
        }
        return url
    }

    private func recordingsDirectory() throws -> URL {
        try Self.recordingsDirectory(fileManager: fileManager)
    }

    private func timestampFormatter() -> DateFormatter {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = .autoupdatingCurrent
        formatter.dateFormat = "yyyy-MM-dd HH.mm.ss"
        return formatter
    }
}

enum AudioRecorderError: LocalizedError {
    case documentsDirectoryUnavailable
    case formatUnavailable
    case bufferUnavailable
    case operationFailed(String, Error)

    var errorDescription: String? {
        switch self {
        case .documentsDirectoryUnavailable:
            "Documents directory unavailable"
        case .formatUnavailable:
            "Recording format unavailable"
        case .bufferUnavailable:
            "Recording buffer unavailable"
        case let .operationFailed(operation, error):
            "\(operation): \(error.localizedDescription)"
        }
    }
}
