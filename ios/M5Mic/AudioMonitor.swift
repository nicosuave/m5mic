import AVFoundation
import Foundation

@MainActor
final class AudioMonitor {
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 1)!
    private var configured = false

    func play(_ samples: [Float]) throws {
        guard !samples.isEmpty else { return }
        do {
            try startIfNeeded()
        } catch {
            throw AudioMonitorError.operationFailed("Start speaker monitor", error)
        }

        guard let buffer = AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: AVAudioFrameCount(samples.count)
        ) else {
            return
        }
        buffer.frameLength = AVAudioFrameCount(samples.count)
        if let channel = buffer.floatChannelData?[0] {
            samples.withUnsafeBufferPointer { source in
                if let baseAddress = source.baseAddress {
                    channel.update(from: baseAddress, count: samples.count)
                }
            }
        }

        player.scheduleBuffer(buffer)
        if !player.isPlaying {
            player.play()
        }
    }

    func stop() {
        player.stop()
        engine.pause()
        try? AVAudioSession.sharedInstance().setActive(false, options: [.notifyOthersOnDeactivation])
    }

    private func startIfNeeded() throws {
        if !configured {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, mode: .default, options: [])
            try session.setActive(true)
            engine.attach(player)
            engine.connect(player, to: engine.mainMixerNode, format: format)
            configured = true
        }
        if !engine.isRunning {
            try engine.start()
        }
    }
}

enum AudioMonitorError: LocalizedError {
    case operationFailed(String, Error)

    var errorDescription: String? {
        switch self {
        case let .operationFailed(operation, error):
            "\(operation): \(error.localizedDescription)"
        }
    }
}
