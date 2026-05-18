import Darwin
import Foundation

final class ModeCommandSender {
    func send(_ mode: M5MicTransportMode) {
        let payloads = RustConstants.controlPayloads(for: mode)
        guard !payloads.isEmpty else { return }

        DispatchQueue.global(qos: .utility).async {
            self.send(payloads)
        }
    }

    func send(_ command: M5MicRecordingCommand) {
        let payloads = RustConstants.controlPayloads(for: command)
        guard !payloads.isEmpty else { return }

        DispatchQueue.global(qos: .utility).async {
            self.send(payloads)
        }
    }

    private func send(_ payloads: [Data]) {
        let descriptor = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
        guard descriptor >= 0 else { return }
        defer { close(descriptor) }

        var enabled: Int32 = 1
        setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_BROADCAST,
            &enabled,
            socklen_t(MemoryLayout<Int32>.size)
        )

        let targets = NetworkAddress.broadcastIPv4Addresses()
        for _ in 0..<8 {
            for target in targets {
                for payload in payloads {
                    send(payload, to: target, descriptor: descriptor)
                }
            }
            usleep(75_000)
        }
    }

    private func send(_ payload: Data, to target: in_addr_t, descriptor: Int32) {
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = in_port_t(RustConstants.controlPort).bigEndian
        address.sin_addr = in_addr(s_addr: target)

        payload.withUnsafeBytes { payloadBuffer in
            guard let payloadBase = payloadBuffer.baseAddress else { return }
            withUnsafePointer(to: &address) { addressPointer in
                addressPointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { socketAddress in
                    _ = sendto(
                        descriptor,
                        payloadBase,
                        payload.count,
                        0,
                        socketAddress,
                        socklen_t(MemoryLayout<sockaddr_in>.size)
                    )
                }
            }
        }
    }
}
