import CoreBluetooth
import Foundation

final class BluetoothModeCommandSender: NSObject {
    private lazy var central = CBCentralManager(delegate: self, queue: .main)
    private var peripheral: CBPeripheral?
    private var controlCharacteristic: CBCharacteristic?
    private var pendingPayloads: [Data] = []
    private var sendID = UUID()

    private let serviceUUID = CBUUID(string: RustConstants.bluetoothServiceUUID)
    private let controlUUID = CBUUID(string: RustConstants.bluetoothControlCharacteristicUUID)

    func send(_ mode: M5MicTransportMode) {
        let payloads = RustConstants.controlPayloads(for: mode)
        send(payloads)
    }

    func send(_ command: M5MicRecordingCommand) {
        let payloads = RustConstants.controlPayloads(for: command)
        send(payloads)
    }

    private func send(_ payloads: [Data]) {
        guard !payloads.isEmpty else { return }

        sendID = UUID()
        pendingPayloads = payloads
        controlCharacteristic = nil
        peripheral = nil

        let currentSendID = sendID
        _ = central
        if central.state == .poweredOn {
            startScan()
        }

        DispatchQueue.main.asyncAfter(deadline: .now() + 6) { [weak self] in
            guard self?.sendID == currentSendID else { return }
            self?.finish()
        }
    }

    private func startScan() {
        guard !pendingPayloads.isEmpty else { return }
        central.stopScan()
        central.scanForPeripherals(
            withServices: [serviceUUID],
            options: [CBCentralManagerScanOptionAllowDuplicatesKey: false]
        )
    }

    private func writeNextPayload() {
        guard
            let peripheral,
            let controlCharacteristic,
            !pendingPayloads.isEmpty
        else {
            finish()
            return
        }

        let payload = pendingPayloads.removeFirst()
        let writeType: CBCharacteristicWriteType = controlCharacteristic.properties.contains(.write)
            ? .withResponse
            : .withoutResponse
        peripheral.writeValue(payload, for: controlCharacteristic, type: writeType)

        if writeType == .withoutResponse {
            DispatchQueue.main.async { [weak self] in
                self?.writeNextPayload()
            }
        }
    }

    private func finish() {
        pendingPayloads.removeAll()
        central.stopScan()
        if let peripheral {
            central.cancelPeripheralConnection(peripheral)
        }
        peripheral = nil
        controlCharacteristic = nil
    }
}

extension BluetoothModeCommandSender: CBCentralManagerDelegate {
    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        if central.state == .poweredOn {
            startScan()
        } else if central.state != .unknown && central.state != .resetting {
            finish()
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
        peripheral.delegate = self
        central.connect(peripheral)
    }

    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
        peripheral.discoverServices([serviceUUID])
    }

    func centralManager(_ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?) {
        finish()
    }

    func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
        if self.peripheral?.identifier == peripheral.identifier {
            self.peripheral = nil
            controlCharacteristic = nil
        }
    }
}

extension BluetoothModeCommandSender: CBPeripheralDelegate {
    func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
        guard error == nil else {
            finish()
            return
        }
        guard let service = peripheral.services?.first(where: { $0.uuid == serviceUUID }) else {
            finish()
            return
        }
        peripheral.discoverCharacteristics([controlUUID], for: service)
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
        guard error == nil else {
            finish()
            return
        }
        guard let characteristic = service.characteristics?.first(where: { characteristic in
            characteristic.uuid == controlUUID
                && (characteristic.properties.contains(.write)
                    || characteristic.properties.contains(.writeWithoutResponse))
        }) else {
            finish()
            return
        }
        controlCharacteristic = characteristic
        writeNextPayload()
    }

    func peripheral(_ peripheral: CBPeripheral, didWriteValueFor characteristic: CBCharacteristic, error: Error?) {
        guard error == nil else {
            finish()
            return
        }
        writeNextPayload()
    }
}
