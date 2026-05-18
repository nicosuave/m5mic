import CryptoKit
import Darwin
import Foundation
import Network

final class WiFiReceiver {
    var onPhase: ((ReceiverPhase) -> Void)?
    var onFrame: ((Data) -> Void)?
    var onDiscoveryReply: (() -> Void)?
    var onHandshake: (() -> Void)?

    private let queue = DispatchQueue(label: "m5mic.wifi.receiver")
    private var tcpListener: NWListener?
    private let discoveryResponder = WiFiDiscoveryResponder()
    private var connections: [UUID: WiFiWebSocketConnection] = [:]
    private var running = false

    var receiverURL: String {
        let host = NetworkAddress.localIPv4Address() ?? "0.0.0.0"
        return "ws://\(host):\(RustConstants.webSocketPort)\(RustConstants.webSocketPath)"
    }

    func start() {
        guard !running else { return }
        running = true
        do {
            try startTCPListener()
            try discoveryResponder.start(
                responseProvider: { [weak self] in
                    guard let self else { return "" }
                    return "\(RustConstants.discoveryResponsePrefix)\(self.receiverURL) source=ios priority=100\n"
                },
                onReply: { [weak self] in
                    self?.onDiscoveryReply?()
                }
            )
            onPhase?(.waiting)
        } catch {
            stop()
            onPhase?(.failed(error.localizedDescription))
        }
    }

    func stop() {
        running = false
        tcpListener?.cancel()
        discoveryResponder.stop()
        tcpListener = nil
        for connection in connections.values {
            connection.cancel()
        }
        connections.removeAll()
        onPhase?(.idle)
    }

    private func startTCPListener() throws {
        let parameters = NWParameters.tcp
        parameters.allowLocalEndpointReuse = true
        guard let port = NWEndpoint.Port(rawValue: RustConstants.webSocketPort) else {
            throw NSError(domain: "m5mic", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid WebSocket port"])
        }

        let listener = try NWListener(using: parameters, on: port)
        listener.service = NWListener.Service(
            name: "M5Mic iPhone",
            type: RustConstants.bonjourServiceType,
            domain: nil,
            txtRecord: NWTXTRecord([
                "path": RustConstants.webSocketPath,
                "codec": "pcm_s16le",
                "codecs": "pcm_s16le,ima_adpcm4",
                "sample_rate": "16000",
                "channels": "1",
                "udp_discovery_port": String(RustConstants.discoveryPort),
                "source": "ios",
                "priority": "100",
            ])
        )
        listener.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                self.onPhase?(.waiting)
            case let .failed(error):
                self.onPhase?(.failed(error.localizedDescription))
                self.stop()
            case .cancelled:
                if self.running {
                    self.onPhase?(.failed("Wi-Fi receiver stopped"))
                }
            default:
                break
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else {
                connection.cancel()
                return
            }
            let id = UUID()
            let webSocket = WiFiWebSocketConnection(id: id, connection: connection, queue: self.queue)
            webSocket.onFrame = { [weak self] frame in
                self?.onFrame?(frame)
            }
            webSocket.onReady = { [weak self] _ in
                self?.onHandshake?()
                self?.onPhase?(.connected)
            }
            webSocket.onClose = { [weak self] id in
                guard let self else { return }
                self.connections.removeValue(forKey: id)
                if self.running, self.connections.isEmpty {
                    self.onPhase?(.waiting)
                }
            }
            self.connections[id] = webSocket
            webSocket.start()
        }
        tcpListener = listener
        listener.start(queue: queue)
    }
}

private final class WiFiDiscoveryResponder {
    private let queue = DispatchQueue(label: "m5mic.wifi.discovery")
    private let lock = NSLock()
    private var descriptor: Int32 = -1
    private var running = false

    func start(responseProvider: @escaping () -> String, onReply: @escaping () -> Void) throws {
        stop()

        let socketDescriptor = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
        guard socketDescriptor >= 0 else {
            throw socketError("create discovery socket")
        }

        do {
            try configure(socketDescriptor)
            try bind(socketDescriptor)
        } catch {
            Darwin.close(socketDescriptor)
            throw error
        }

        lock.lock()
        descriptor = socketDescriptor
        running = true
        lock.unlock()

        queue.async { [weak self] in
            self?.receiveLoop(
                descriptor: socketDescriptor,
                responseProvider: responseProvider,
                onReply: onReply
            )
        }
    }

    func stop() {
        lock.lock()
        let socketDescriptor = descriptor
        descriptor = -1
        running = false
        lock.unlock()

        if socketDescriptor >= 0 {
            Darwin.close(socketDescriptor)
        }
    }

    private func configure(_ socketDescriptor: Int32) throws {
        var enabled: Int32 = 1
        guard setsockopt(
            socketDescriptor,
            SOL_SOCKET,
            SO_REUSEADDR,
            &enabled,
            socklen_t(MemoryLayout<Int32>.size)
        ) == 0 else {
            throw socketError("enable discovery socket reuse")
        }

        _ = setsockopt(
            socketDescriptor,
            SOL_SOCKET,
            SO_REUSEPORT,
            &enabled,
            socklen_t(MemoryLayout<Int32>.size)
        )

        var timeout = timeval(tv_sec: 0, tv_usec: 250_000)
        guard setsockopt(
            socketDescriptor,
            SOL_SOCKET,
            SO_RCVTIMEO,
            &timeout,
            socklen_t(MemoryLayout<timeval>.size)
        ) == 0 else {
            throw socketError("set discovery socket timeout")
        }
    }

    private func bind(_ socketDescriptor: Int32) throws {
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = in_port_t(RustConstants.discoveryPort).bigEndian
        address.sin_addr = in_addr(s_addr: INADDR_ANY)

        let result = withUnsafePointer(to: &address) { addressPointer in
            addressPointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { socketAddress in
                Darwin.bind(
                    socketDescriptor,
                    socketAddress,
                    socklen_t(MemoryLayout<sockaddr_in>.size)
                )
            }
        }
        guard result == 0 else {
            throw socketError("bind discovery socket")
        }
    }

    private func receiveLoop(
        descriptor socketDescriptor: Int32,
        responseProvider: @escaping () -> String,
        onReply: @escaping () -> Void
    ) {
        var buffer = [UInt8](repeating: 0, count: 512)
        while isRunning(socketDescriptor) {
            var sender = sockaddr_storage()
            var senderLength = socklen_t(MemoryLayout<sockaddr_storage>.size)
            let bytesRead = buffer.withUnsafeMutableBytes { rawBuffer in
                withUnsafeMutablePointer(to: &sender) { senderPointer in
                    senderPointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { socketAddress in
                        recvfrom(
                            socketDescriptor,
                            rawBuffer.baseAddress,
                            rawBuffer.count,
                            0,
                            socketAddress,
                            &senderLength
                        )
                    }
                }
            }

            guard bytesRead > 0 else {
                continue
            }

            let request = Data(buffer.prefix(bytesRead))
            guard request.starts(with: RustConstants.discoveryRequest) else {
                continue
            }

            let response = responseProvider()
            guard !response.isEmpty else {
                continue
            }

            let sent = send(response, to: &sender, senderLength: senderLength, descriptor: socketDescriptor)
            if sent {
                onReply()
            }
        }
    }

    private func send(
        _ response: String,
        to sender: inout sockaddr_storage,
        senderLength: socklen_t,
        descriptor socketDescriptor: Int32
    ) -> Bool {
        let responseData = Data(response.utf8)
        let result = responseData.withUnsafeBytes { responseBuffer in
            guard let responseBase = responseBuffer.baseAddress else { return -1 }
            return withUnsafePointer(to: &sender) { senderPointer in
                senderPointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { socketAddress in
                    sendto(
                        socketDescriptor,
                        responseBase,
                        responseData.count,
                        0,
                        socketAddress,
                        senderLength
                    )
                }
            }
        }
        return result == responseData.count
    }

    private func isRunning(_ socketDescriptor: Int32) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return running && descriptor == socketDescriptor
    }

    private func socketError(_ operation: String) -> NSError {
        let code = errno
        let message = String(cString: strerror(code))
        return NSError(
            domain: NSPOSIXErrorDomain,
            code: Int(code),
            userInfo: [NSLocalizedDescriptionKey: "\(operation): \(message)"]
        )
    }
}

private final class WiFiWebSocketConnection {
    let id: UUID
    var onReady: ((UUID) -> Void)?
    var onFrame: ((Data) -> Void)?
    var onClose: ((UUID) -> Void)?

    private let connection: NWConnection
    private let queue: DispatchQueue
    private var handshakeBuffer = Data()
    private var frameBuffer = Data()
    private var closed = false

    init(id: UUID, connection: NWConnection, queue: DispatchQueue) {
        self.id = id
        self.connection = connection
        self.queue = queue
    }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            if case .cancelled = state {
                self?.close()
            } else if case .failed = state {
                self?.close()
            }
        }
        connection.start(queue: queue)
        receiveHandshake()
    }

    func cancel() {
        connection.cancel()
        close()
    }

    private func receiveHandshake() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 4_096) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if let data {
                self.handshakeBuffer.append(data)
            }
            if error != nil || isComplete {
                self.close()
                return
            }
            if self.completeHandshakeIfPossible() {
                self.receiveFrames()
            } else {
                self.receiveHandshake()
            }
        }
    }

    private func completeHandshakeIfPossible() -> Bool {
        let separator = Data("\r\n\r\n".utf8)
        guard let range = handshakeBuffer.range(of: separator) else { return false }
        let headerData = handshakeBuffer[..<range.lowerBound]
        let remaining = Data(handshakeBuffer[range.upperBound...])
        guard
            let header = String(data: headerData, encoding: .utf8),
            let key = webSocketKey(in: header),
            header.hasPrefix("GET \(RustConstants.webSocketPath)")
        else {
            close()
            return true
        }

        let accept = webSocketAccept(for: key)
        let response = "HTTP/1.1 101 Switching Protocols\r\n"
            + "Upgrade: websocket\r\n"
            + "Connection: Upgrade\r\n"
            + "Sec-WebSocket-Accept: \(accept)\r\n"
            + "\r\n"
        connection.send(content: Data(response.utf8), completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if error == nil {
                self.onReady?(self.id)
            } else {
                self.close()
            }
        })
        frameBuffer.append(remaining)
        parseFrames()
        return true
    }

    private func receiveFrames() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 4_096) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if let data {
                self.frameBuffer.append(data)
                self.parseFrames()
            }
            if error != nil || isComplete {
                self.close()
            } else {
                self.receiveFrames()
            }
        }
    }

    private func parseFrames() {
        while frameBuffer.count >= 2 {
            let first = frameBuffer[0]
            let second = frameBuffer[1]
            let opcode = first & 0x0f
            let masked = second & 0x80 != 0
            var offset = 2
            var payloadLength = Int(second & 0x7f)

            if payloadLength == 126 {
                guard frameBuffer.count >= offset + 2 else { return }
                payloadLength = (Int(frameBuffer[offset]) << 8) | Int(frameBuffer[offset + 1])
                offset += 2
            } else if payloadLength == 127 {
                guard frameBuffer.count >= offset + 8 else { return }
                var length: UInt64 = 0
                for index in 0..<8 {
                    length = (length << 8) | UInt64(frameBuffer[offset + index])
                }
                guard length <= UInt64(Int.max) else {
                    close()
                    return
                }
                payloadLength = Int(length)
                offset += 8
            }

            var mask: [UInt8] = []
            if masked {
                guard frameBuffer.count >= offset + 4 else { return }
                mask = Array(frameBuffer[offset..<offset + 4])
                offset += 4
            }

            guard frameBuffer.count >= offset + payloadLength else { return }
            let payloadRange = offset..<offset + payloadLength
            var payload = Data(frameBuffer[payloadRange])
            frameBuffer.removeSubrange(0..<offset + payloadLength)

            if masked {
                var bytes = Array(payload)
                for index in bytes.indices {
                    bytes[index] ^= mask[index % 4]
                }
                payload = Data(bytes)
            }

            switch opcode {
            case 0x2:
                onFrame?(payload)
            case 0x8:
                sendFrame(opcode: 0x8, payload: Data())
                close()
                return
            case 0x9:
                sendFrame(opcode: 0xA, payload: payload)
            default:
                break
            }
        }
    }

    private func sendFrame(opcode: UInt8, payload: Data) {
        guard payload.count < 126 else { return }
        var frame = Data([0x80 | opcode, UInt8(payload.count)])
        frame.append(payload)
        connection.send(content: frame, completion: .contentProcessed { _ in })
    }

    private func close() {
        guard !closed else { return }
        closed = true
        connection.cancel()
        onClose?(id)
    }

    private func webSocketKey(in header: String) -> String? {
        for line in header.components(separatedBy: "\r\n") {
            let parts = line.split(separator: ":", maxSplits: 1, omittingEmptySubsequences: false)
            guard parts.count == 2 else { continue }
            if parts[0].trimmingCharacters(in: .whitespaces).lowercased() == "sec-websocket-key" {
                return parts[1].trimmingCharacters(in: .whitespaces)
            }
        }
        return nil
    }

    private func webSocketAccept(for key: String) -> String {
        let guid = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        let digest = Insecure.SHA1.hash(data: Data((key + guid).utf8))
        return Data(digest).base64EncodedString()
    }
}
