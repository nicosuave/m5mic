import SwiftUI
import UniformTypeIdentifiers

struct ContentView: View {
    @State private var store = M5MicStore()

    var body: some View {
        NavigationStack {
            List {
                ModeSection(store: store)
                StatusSection(store: store)
                if store.selectedTransport != .usb {
                    RecordingSection(store: store)
                    StreamSection(store: store)
                }
            }
            .navigationTitle("m5mic")
        }
    }
}

private struct StatusSection: View {
    let store: M5MicStore

    var body: some View {
        Section {
            HStack(spacing: 16) {
                Image(systemName: store.phase.systemImage)
                    .font(.system(size: 32, weight: .semibold))
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(statusColor)
                    .frame(width: 44, height: 44)

                VStack(alignment: .leading, spacing: 4) {
                    Text(store.phase.title)
                        .font(.headline)
                    Text(store.selectedTransport.title)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }

                Spacer()

                Gauge(value: store.inputLevel, in: 0...1) {
                    Text("Level")
                }
                .gaugeStyle(.accessoryCircularCapacity)
                .tint(statusColor)
                .frame(width: 56, height: 56)
            }
            .padding(.vertical, 6)
        }
    }

    private var statusColor: Color {
        switch store.phase {
        case .receiving:
            .red
        case .failed:
            .orange
        case .connected, .waiting, .scanning, .connecting:
            .blue
        case .idle, .usbMode:
            .secondary
        }
    }
}

private struct ModeSection: View {
    let store: M5MicStore

    var body: some View {
        @Bindable var store = store

        Section("Mode") {
            Picker("Mode", selection: $store.selectedTransport) {
                ForEach(M5MicTransport.allCases) { transport in
                    Label(transport.title, systemImage: transport.systemImage)
                        .tag(transport)
                }
            }
            .pickerStyle(.segmented)

            if store.selectedTransport != .usb {
                Toggle(isOn: $store.monitorEnabled) {
                    Label("Speaker Monitor", systemImage: "speaker.wave.2")
                }
            }
        }
    }
}

private struct StreamSection: View {
    let store: M5MicStore

    var body: some View {
        Section("Stream") {
            Toggle(isOn: Binding(
                get: { store.deviceRecordingRequested },
                set: { store.setDeviceRecordingEnabled($0) }
            )) {
                Label("Mic Recording", systemImage: "record.circle")
            }
            .tint(.red)

            LabeledContent("Mic", value: store.deviceRecordingStatusText)
            LabeledContent("Stream ID", value: store.streamIDText)
                .monospacedDigit()
            LabeledContent("Frames", value: store.frameCount.formatted())
            LabeledContent("Samples", value: store.sampleCount.formatted())
            if store.selectedTransport == .wifi {
                LabeledContent("Discovery Replies", value: store.wifiDiscoveryReplyCount.formatted())
                LabeledContent("WebSocket Handshakes", value: store.wifiHandshakeCount.formatted())
                LabeledContent("Wi-Fi URL", value: store.receiverURL)
                    .font(.caption)
                    .textSelection(.enabled)
            }
        }
    }
}

private struct RecordingSection: View {
    let store: M5MicStore
    @State private var isChoosingFolder = false
    @State private var isBrowsingFolder = false

    var body: some View {
        @Bindable var store = store

        Section("Recording") {
            Button {
                isChoosingFolder = true
            } label: {
                Label(
                    store.recordingDestinationConfigured ? "Change Files Folder" : "Choose Files Folder",
                    systemImage: "folder.badge.plus"
                )
            }

            Button {
                isBrowsingFolder = true
            } label: {
                HStack {
                    Label("Files Folder", systemImage: "folder")
                    Spacer()
                    Text(store.recordingDestinationName)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    Image(systemName: "chevron.right")
                        .font(.footnote.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }
            }
            .disabled(store.recordingFolderURL == nil)

            Toggle(isOn: Binding(
                get: { store.recordingEnabled },
                set: { store.setRecordingEnabled($0) }
            )) {
                Label("Record Audio", systemImage: "record.circle")
            }
            .tint(.red)
            .disabled(!store.recordingDestinationConfigured)

            LabeledContent("Status", value: store.recordingStatusText)
            LabeledContent("Recordings", value: store.savedRecordingCount.formatted())
            LabeledContent("Files Saved", value: store.filesSavedCount.formatted())
            LabeledContent("Samples", value: store.recordingSampleCount.formatted())
        }
        .fileImporter(
            isPresented: $isChoosingFolder,
            allowedContentTypes: [.folder],
            allowsMultipleSelection: false
        ) { result in
            switch result {
            case let .success(urls):
                if let url = urls.first {
                    store.selectRecordingFolder(url)
                }
            case let .failure(error):
                store.recordingError = error.localizedDescription
            }
        }
        .sheet(isPresented: $isBrowsingFolder) {
            RecordingFolderBrowser(directoryURL: store.recordingFolderURL) { message in
                store.recordingError = message
            }
        }
        .alert("Recording Error", isPresented: Binding(
            get: { store.recordingError != nil },
            set: { if !$0 { store.clearRecordingError() } }
        )) {
            Button("OK", role: .cancel) {
                store.clearRecordingError()
            }
        } message: {
            Text(store.recordingError ?? "")
        }
    }
}

#Preview {
    ContentView()
}
