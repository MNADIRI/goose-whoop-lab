import SwiftUI
import UniformTypeIdentifiers

struct ConnectionView: View {
  @EnvironmentObject private var model: GooseAppModel

  var body: some View {
    ConnectionContentView(ble: model.ble)
      .environmentObject(model)
  }
}

/// whoop-lab: export folder, auto-sync toggle and manual export.
private struct WhoopLabExportSection: View {
  @ObservedObject var export: WhoopLabAutoExport
  @State private var pickingFolder = false

  var body: some View {
    Section("whoop-lab") {
      LabeledContent("Folder", value: export.folderName ?? "None")
      LabeledContent("Export", value: export.status)
      Toggle("Auto history sync (30 min)", isOn: $export.autoHistoricalSync)
      Button(export.folderName == nil ? "Choose Export Folder…" : "Change Export Folder…") {
        pickingFolder = true
      }
      Button("Export Now") {
        export.exportNow(reason: "manual")
      }
      .disabled(export.folderName == nil)
    }
    .fileImporter(isPresented: $pickingFolder, allowedContentTypes: [.folder]) { result in
      if case .success(let url) = result {
        export.setFolder(url)
      }
    }
  }
}

private struct ConnectionContentView: View {
  @EnvironmentObject private var model: GooseAppModel
  @EnvironmentObject private var messageStore: GooseMessageStore
  @ObservedObject var ble: GooseBLEClient

  var body: some View {
    List {
      Section("Status") {
        LabeledContent("Bluetooth", value: ble.bluetoothState)
        LabeledContent("Connection", value: ble.connectionState)
        LabeledContent("Reconnect", value: ble.reconnectState)
        LabeledContent("Historical", value: historicalSyncValue)
        LabeledContent("Remembered", value: ble.rememberedDeviceDescription)
        LabeledContent("Live HR", value: liveHeartRateValue)
        LabeledContent("Rust", value: model.rustStatus)
        LabeledContent("Hello", value: model.helloSummary)
      }

      WhoopLabExportSection(export: model.whoopLabExport)

      Section("Actions") {
        Button("Request Bluetooth") {
          ble.requestBluetooth()
        }
        Button(ble.isScanning ? "Stop Scan" : "Scan") {
          ble.isScanning ? ble.stopScan() : ble.startScan()
        }
        .disabled(!ble.canScan)

        Button("Connect Selected") {
          ble.connectSelected()
        }
        .disabled(!ble.canConnect)

        Button("Reconnect Remembered") {
          ble.reconnectRemembered()
        }
        .disabled(!ble.canReconnectRemembered)

        Button("Send Client Hello") {
          ble.sendClientHello()
        }
        .disabled(!ble.canSendHello)

        Button(ble.isHistoricalSyncing ? "Syncing Historical Packets" : "Request Historical Packets") {
          ble.syncHistoricalPackets()
        }
        .disabled(!ble.canSyncHistorical)

        Button("Forget Remembered Device", role: .destructive) {
          ble.forgetRememberedDevice()
        }
        .disabled(!ble.hasRememberedDevice)
      }

      Section("Discovered") {
        if ble.discoveredDevices.isEmpty {
          Text("No devices yet")
            .foregroundStyle(.secondary)
        } else {
          ForEach(ble.discoveredDevices) { device in
            Button {
              ble.select(device)
            } label: {
              HStack {
                VStack(alignment: .leading) {
                  Text(device.name)
                  Text(device.id.uuidString)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                }
                Spacer()
                Text("\(device.rssi)")
                  .foregroundStyle(.secondary)
              }
            }
          }
        }
      }

      Section("Client Hello") {
        Text(GooseHello.clientHelloFrameHex)
          .font(.system(.footnote, design: .monospaced))
          .textSelection(.enabled)
      }

      Section("Event Log") {
        ForEach(messageStore.messages) { message in
          VStack(alignment: .leading, spacing: 4) {
            HStack {
              Text(message.timestamp, style: .time)
                .font(.caption)
                .foregroundStyle(.secondary)
              Text(message.level.rawValue.uppercased())
                .font(.caption.weight(.semibold))
                .foregroundStyle(message.level == .error ? .red : .secondary)
              Text(message.source)
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            Text(message.title)
              .font(.subheadline.weight(.semibold))
            Text(message.body)
              .font(.system(.caption, design: .monospaced))
              .foregroundStyle(.secondary)
              .textSelection(.enabled)
          }
        }
      }
    }
    .gooseListBackground()
    .navigationTitle("Connect")
  }

  private var liveHeartRateValue: String {
    guard let bpm = ble.liveHeartRateBPM else {
      return ble.liveHeartRateSource
    }
    if let updatedAt = ble.liveHeartRateUpdatedAt {
      return "\(bpm) bpm via \(ble.liveHeartRateSource) @ \(updatedAt.formatted(date: .omitted, time: .standard))"
    }
    return "\(bpm) bpm via \(ble.liveHeartRateSource)"
  }

  private var historicalSyncValue: String {
    let packetCount = ble.historicalPacketCount
    let packets = "\(packetCount) \(packetCount == 1 ? "packet" : "packets")"
    if ble.isHistoricalSyncing {
      return "syncing | \(packets)"
    }
    if let completedAt = ble.lastHistoricalSyncCompletedAt {
      return "\(ble.historicalSyncStatus) | \(packets) @ \(completedAt.formatted(date: .omitted, time: .standard))"
    }
    return "\(ble.historicalSyncStatus) | \(packets)"
  }
}
