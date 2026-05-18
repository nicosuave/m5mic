import Foundation

@MainActor
final class RecordingDestination {
    private let bookmarkKey = "M5MicRecordingDestinationBookmark"
    private let fileManager = FileManager.default
    private let defaults: UserDefaults

    private(set) var folderURL: URL?

    var isConfigured: Bool {
        folderURL != nil
    }

    var folderName: String {
        folderURL?.lastPathComponent ?? "Not selected"
    }

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        restoreBookmark()
        if folderURL == nil {
            folderURL = try? AudioRecorder.recordingsDirectory(fileManager: fileManager)
        }
    }

    func selectFolder(_ url: URL) throws {
        let accessed = url.startAccessingSecurityScopedResource()
        defer {
            if accessed {
                url.stopAccessingSecurityScopedResource()
            }
        }

        let bookmark: Data
        do {
            bookmark = try url.bookmarkData(
                options: [],
                includingResourceValuesForKeys: nil,
                relativeTo: nil
            )
        } catch {
            throw RecordingDestinationError.operationFailed("Save Files folder access", error)
        }
        defaults.set(bookmark, forKey: bookmarkKey)
        folderURL = url
    }

    func copyRecording(_ sourceURL: URL) throws -> Bool {
        guard let folderURL else {
            throw RecordingDestinationError.folderNotSelected
        }

        let accessed = folderURL.startAccessingSecurityScopedResource()
        defer {
            if accessed {
                folderURL.stopAccessingSecurityScopedResource()
            }
        }

        var isDirectory: ObjCBool = false
        guard fileManager.fileExists(atPath: folderURL.path, isDirectory: &isDirectory), isDirectory.boolValue else {
            throw RecordingDestinationError.folderUnavailable
        }

        guard let destinationURL = try destinationURL(for: sourceURL, in: folderURL) else {
            return true
        }
        do {
            try fileManager.copyItem(at: sourceURL, to: destinationURL)
        } catch {
            throw RecordingDestinationError.operationFailed("Copy recording to Files", error)
        }
        return true
    }

    private func restoreBookmark() {
        guard let bookmark = defaults.data(forKey: bookmarkKey) else { return }

        do {
            var stale = false
            let url = try URL(
                resolvingBookmarkData: bookmark,
                options: [],
                relativeTo: nil,
                bookmarkDataIsStale: &stale
            )
            folderURL = url
            if stale {
                try selectFolder(url)
            }
        } catch {
            defaults.removeObject(forKey: bookmarkKey)
            folderURL = nil
        }
    }

    private func destinationURL(for sourceURL: URL, in folderURL: URL) throws -> URL? {
        let baseName = sourceURL.deletingPathExtension().lastPathComponent
        let fileExtension = sourceURL.pathExtension
        var candidate = folderURL.appendingPathComponent(sourceURL.lastPathComponent)

        if !fileManager.fileExists(atPath: candidate.path) {
            return candidate
        }

        if try filesHaveSameSize(sourceURL, candidate) {
            return nil
        }

        var suffix = 2
        repeat {
            candidate = folderURL.appendingPathComponent("\(baseName) \(suffix).\(fileExtension)")
            suffix += 1
        } while fileManager.fileExists(atPath: candidate.path)

        return candidate
    }

    private func filesHaveSameSize(_ lhs: URL, _ rhs: URL) throws -> Bool {
        let lhsSize: NSNumber?
        let rhsSize: NSNumber?
        do {
            lhsSize = try fileManager.attributesOfItem(atPath: lhs.path)[.size] as? NSNumber
            rhsSize = try fileManager.attributesOfItem(atPath: rhs.path)[.size] as? NSNumber
        } catch {
            throw RecordingDestinationError.operationFailed("Inspect existing recording", error)
        }
        return lhsSize == rhsSize
    }
}

enum RecordingDestinationError: LocalizedError {
    case folderNotSelected
    case folderUnavailable
    case operationFailed(String, Error)

    var errorDescription: String? {
        switch self {
        case .folderNotSelected:
            "Choose a Files folder before recording."
        case .folderUnavailable:
            "The selected Files folder is unavailable."
        case let .operationFailed(operation, error):
            "\(operation): \(error.localizedDescription)"
        }
    }
}
