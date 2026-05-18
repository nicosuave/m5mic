import CoreBluetooth
import Foundation

final class BluetoothReceiver: NSObject {
    var onPhase: ((ReceiverPhase) -> Void)?
    var onFrame: ((Data) -> Void)?

    private lazy var central = CBCentralManager(delegate: self, queue: .main)
    private var peripheral: CBPeripheral?
    private var audioCharacteristic: CBCharacteristic?
    private var controlCharacteristic: CBCharacteristic?
    private var pendingControlPayloads: [Data] = []
    private var reassembler: BleFrameReassembler?
    private var stopping = false

    private let serviceUUID = CBUUID(string: RustConstants.bluetoothServiceUUID)
    private let audioUUID = CBUUID(string: RustConstants.bluetoothAudioCharacteristicUUID)
    private let controlUUID = CBUUID(string: RustConstants.bluetoothControlCharacteristicUUID)
    private let statusUUID = CBUUID(string: RustConstants.bluetoothStatusCharacteristicUUID)

    func start() {
        stopping = false
        if reassembler == nil {
            reassembler = try? BleFrameReassembler()
        }

        switch central.state {
        case .poweredOn:
            scan()
        case .unknown, .resetting:
            onPhase?(.scanning)
        case .unsupported:
            onPhase?(.failed("Bluetooth unsupported"))
        case .unauthorized:
            onPhase?(.failed("Bluetooth permission denied"))
        case .poweredOff:
            onPhase?(.failed("Bluetooth off"))
        @unknown default:
            onPhase?(.failed("Bluetooth unavailable"))
        }
    }

    func stop() {
        stopping = true
        central.stopScan()
        if let peripheral {
            if let audioCharacteristic {
                peripheral.setNotifyValue(false, for: audioCharacteristic)
            }
            central.cancelPeripheralConnection(peripheral)
        }
        peripheral = nil
        audioCharacteristic = nil
        controlCharacteristic = nil
        pendingControlPayloads.removeAll()
        reassembler?.reset()
        onPhase?(.idle)
    }

    func sendMode(_ mode: M5MicTransportMode) {
        let payloads = RustConstants.controlPayloads(for: mode)
        sendControlPayloads(payloads)
    }

    func sendRecordingCommand(_ command: M5MicRecordingCommand) {
        let payloads = RustConstants.controlPayloads(for: command)
        sendControlPayloads(payloads)
    }

    private func sendControlPayloads(_ payloads: [Data]) {
        guard !payloads.isEmpty else { return }
        pendingControlPayloads = payloads
        if let peripheral, let controlCharacteristic {
            writeNextControlPayload(peripheral: peripheral, characteristic: controlCharacteristic)
        } else if central.state == .poweredOn, !stopping {
            scan()
        }
    }

    private func writeNextControlPayload(peripheral: CBPeripheral, characteristic: CBCharacteristic) {
        guard !pendingControlPayloads.isEmpty else { return }

        let payload = pendingControlPayloads.removeFirst()
        let writeType: CBCharacteristicWriteType = characteristic.properties.contains(.write)
            ? .withResponse
            : .withoutResponse
        peripheral.writeValue(payload, for: characteristic, type: writeType)

        if writeType == .withoutResponse {
            DispatchQueue.main.async { [weak self, weak peripheral, weak characteristic] in
                guard let self, let peripheral, let characteristic else { return }
                self.writeNextControlPayload(peripheral: peripheral, characteristic: characteristic)
            }
        }
    }

    private func scan() {
        onPhase?(.scanning)
        central.scanForPeripherals(
            withServices: [serviceUUID],
            options: [CBCentralManagerScanOptionAllowDuplicatesKey: false]
        )
    }
}

extension BluetoothReceiver: CBCentralManagerDelegate {
    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        if central.state == .poweredOn, !stopping {
            scan()
        } else if !stopping {
            start()
        }
    }

    func centralManager(
        _ central: CBCentralManager,
        didDiscover peripheral: CBPeripheral,
        advertisementData: [String: Any],
        rssi RSSI: NSNumber
    ) {
        self.peripheral = peripheral
        central.stopScan()
        onPhase?(.connecting)
        peripheral.delegate = self
        central.connect(peripheral)
    }

    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
        onPhase?(.connecting)
        peripheral.discoverServices([serviceUUID])
    }

    func centralManager(_ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?) {
        onPhase?(.failed(error?.localizedDescription ?? "Bluetooth connect failed"))
        if !stopping {
            scan()
        }
    }

    func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
        self.peripheral = nil
        audioCharacteristic = nil
        controlCharacteristic = nil
        pendingControlPayloads.removeAll()
        reassembler?.reset()

        if stopping {
            onPhase?(.idle)
        } else if let error {
            onPhase?(.failed(error.localizedDescription))
            scan()
        } else {
            onPhase?(.scanning)
            scan()
        }
    }
}

extension BluetoothReceiver: CBPeripheralDelegate {
    func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
        if let error {
            onPhase?(.failed(error.localizedDescription))
            return
        }
        guard let service = peripheral.services?.first(where: { $0.uuid == serviceUUID }) else {
            onPhase?(.failed("m5mic service missing"))
            return
        }
        peripheral.discoverCharacteristics([audioUUID, controlUUID, statusUUID], for: service)
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
        if let error {
            onPhase?(.failed(error.localizedDescription))
            return
        }

        for characteristic in service.characteristics ?? [] {
            if characteristic.uuid == audioUUID {
                audioCharacteristic = characteristic
            } else if characteristic.uuid == controlUUID {
                controlCharacteristic = characteristic
            }
        }

        guard let audioCharacteristic else {
            onPhase?(.failed("Bluetooth audio characteristic missing"))
            return
        }
        peripheral.setNotifyValue(true, for: audioCharacteristic)
    }

    func peripheral(_ peripheral: CBPeripheral, didUpdateNotificationStateFor characteristic: CBCharacteristic, error: Error?) {
        if let error {
            onPhase?(.failed(error.localizedDescription))
            return
        }
        if characteristic.uuid == audioUUID, characteristic.isNotifying {
            onPhase?(.connected)
            if let controlCharacteristic, !pendingControlPayloads.isEmpty {
                writeNextControlPayload(peripheral: peripheral, characteristic: controlCharacteristic)
            }
        }
    }

    func peripheral(_ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic, error: Error?) {
        if let error {
            onPhase?(.failed(error.localizedDescription))
            return
        }
        guard characteristic.uuid == audioUUID, let value = characteristic.value else { return }

        do {
            if let frame = try reassembler?.push(value) {
                onFrame?(frame)
            }
        } catch {
            onPhase?(.failed(error.localizedDescription))
            reassembler?.reset()
        }
    }

    func peripheral(_ peripheral: CBPeripheral, didWriteValueFor characteristic: CBCharacteristic, error: Error?) {
        if error != nil {
            pendingControlPayloads.removeAll()
            return
        }
        if characteristic.uuid == controlUUID {
            writeNextControlPayload(peripheral: peripheral, characteristic: characteristic)
        }
    }
}
