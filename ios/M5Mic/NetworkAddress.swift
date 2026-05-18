import Darwin
import Foundation

enum NetworkAddress {
    static func localIPv4Address() -> String? {
        preferredIPv4Records().first.map { string(fromNetworkOrder: $0.address) }
    }

    static func broadcastIPv4Addresses() -> [in_addr_t] {
        var addresses = Set<in_addr_t>([INADDR_BROADCAST])
        for record in preferredIPv4Records() {
            let ip = UInt32(bigEndian: record.address)
            let mask = UInt32(bigEndian: record.netmask)
            addresses.insert((ip | ~mask).bigEndian)
            if let broadcast = record.broadcast {
                addresses.insert(broadcast)
            }
        }
        return Array(addresses)
    }

    private struct IPv4Record {
        let name: String
        let address: in_addr_t
        let netmask: in_addr_t
        let broadcast: in_addr_t?
    }

    private static func preferredIPv4Records() -> [IPv4Record] {
        interfaceIPv4Records().sorted { lhs, rhs in
            score(lhs.name) < score(rhs.name)
        }
    }

    private static func score(_ name: String) -> Int {
        switch name {
        case "en0":
            0
        case let value where value.hasPrefix("en"):
            1
        case let value where value.hasPrefix("pdp_ip"):
            3
        default:
            2
        }
    }

    private static func interfaceIPv4Records() -> [IPv4Record] {
        var ifaddr: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&ifaddr) == 0, let first = ifaddr else { return [] }
        defer { freeifaddrs(first) }

        var records: [IPv4Record] = []
        var cursor: UnsafeMutablePointer<ifaddrs>? = first
        while let current = cursor {
            defer { cursor = current.pointee.ifa_next }

            let interface = current.pointee
            guard let addressPointer = interface.ifa_addr else { continue }
            guard Int32(addressPointer.pointee.sa_family) == AF_INET else { continue }
            let flags = Int32(interface.ifa_flags)
            guard flags & IFF_UP != 0, flags & IFF_LOOPBACK == 0 else { continue }
            guard let netmaskPointer = interface.ifa_netmask else { continue }

            let address = addressPointer.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { pointer in
                pointer.pointee.sin_addr.s_addr
            }
            let netmask = netmaskPointer.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { pointer in
                pointer.pointee.sin_addr.s_addr
            }
            let broadcast = interface.ifa_dstaddr?.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { pointer in
                pointer.pointee.sin_addr.s_addr
            }
            let name = String(cString: interface.ifa_name)

            records.append(IPv4Record(name: name, address: address, netmask: netmask, broadcast: broadcast))
        }
        return records
    }

    private static func string(fromNetworkOrder address: in_addr_t) -> String {
        var address = in_addr(s_addr: address)
        var buffer = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
        return withUnsafePointer(to: &address) { pointer in
            inet_ntop(AF_INET, pointer, &buffer, socklen_t(INET_ADDRSTRLEN))
            return String(cString: buffer)
        }
    }
}
