// Devices — LAN multi-device viewing (core::peer). This Mac, the devices it
// has paired with (status, last sync, enable / forget), possible double
// counting, and devices found nearby over Bonjour. Pairing and discovery run
// in core off the main actor; every failure arrives as a fixed code that
// AppModel.peerMessage puts into words.
import SwiftUI

struct DevicesView: View {
    @Environment(AppModel.self) private var model
    @ViewState private var pairing: PairTarget?
    @ViewState private var forgetting: PeerDevice?

    var body: some View {
        Form {
            thisMacSection.cascade(0)
            pairedSection.cascade(1)
            if let warnings = model.peers?.warnings, !warnings.isEmpty {
                warningsSection(warnings).cascade(2)
            }
            nearbySection.cascade(3)
        }
        .formStyle(.grouped)
        .task { await model.loadPeers() }
        .sheet(item: Binding(get: { pairing }, set: { pairing = $0 })) { target in
            AddDeviceSheet(initialAddress: target.address)
        }
        .confirmationDialog(
            "Forget \(forgetting?.name ?? "")?",
            isPresented: Binding(get: { forgetting != nil }, set: { if !$0 { forgetting = nil } }),
            presenting: forgetting
        ) { device in
            Button("Forget Device", role: .destructive) { model.forgetDevice(device.deviceId) }
        } message: { _ in
            Text("Its synced usage is removed from this Mac. To see it again, pair with a new code.")
        }
    }

    // MARK: Sections

    private var thisMacSection: some View {
        Section("This Mac") {
            LabeledContent("Name", value: model.peers?.device.displayName ?? "—")
            LabeledContent("Device ID") {
                Text(model.peers.map { String($0.device.deviceId.prefix(8)) } ?? "—")
                    .monospaced().textSelection(.enabled)
            }
            Text("Other devices can see this Mac's usage while it shares with `globaltokentracker-cli peer serve`. Sharing from the app is coming soon.")
                .scaledFont(.caption).foregroundStyle(.secondary)
        }
    }

    private var pairedSection: some View {
        Section {
            let peers = model.peers?.peers ?? []
            if peers.isEmpty {
                Text("No paired devices yet. On the other device, start sharing to get a pairing code, then choose Add Device.")
                    .foregroundStyle(.secondary)
            }
            ForEach(peers) { p in
                PairedDeviceRow(device: p, span: span(p.deviceId)) { forgetting = p }
            }
            if let err = model.peerError {
                Label(err, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange).scaledFont(.callout)
            }
        } header: {
            HStack {
                Text("Paired Devices")
                Spacer()
                if model.isSyncingPeers { ProgressView().controlSize(.small) }
                Button("Sync Now") { model.syncPeers() }
                    .disabled(model.isSyncingPeers || (model.peers?.peers.isEmpty ?? true))
                Button("Add Device…") { pairing = PairTarget(address: "") }
            }
        } footer: {
            Text("Devices share hourly totals only — never prompts, sessions, projects or paths. Quotas are per account and never added up across devices.")
                .scaledFont(.caption).foregroundStyle(.secondary)
        }
    }

    private func warningsSection(_ warnings: [OverlapWarning]) -> some View {
        Section("Possible Double Counting") {
            ForEach(warnings) { w in
                Label {
                    Text("\(name(w.deviceA)) and \(name(w.deviceB)) report identical \(Fmt.app(w.app)) usage on \(w.days) days. They may scan the same synced folder; totals still count both.")
                } icon: {
                    Image(systemName: "exclamationmark.2").foregroundStyle(.orange)
                }
                .scaledFont(.callout)
            }
        }
    }

    private var nearbySection: some View {
        Section {
            if model.nearby.isEmpty {
                Text(model.isDiscovering ? "Looking for devices…" : "Choose Find Devices to look for GlobalTokenTracker devices sharing on this network.")
                    .foregroundStyle(.secondary)
            }
            ForEach(model.nearby) { d in
                NearbyRow(device: d) { pairing = PairTarget(address: d.addresses.first ?? "") }
            }
        } header: {
            HStack {
                Text("Nearby")
                Spacer()
                if model.isDiscovering { ProgressView().controlSize(.small) }
                Button("Find Devices") { model.discover() }.disabled(model.isDiscovering)
            }
        } footer: {
            Text("Not listed? Guest Wi-Fi, VLANs and client isolation block discovery — add the device by its address instead, e.g. 192.168.1.20:4319.")
                .scaledFont(.caption).foregroundStyle(.secondary)
        }
    }

    // MARK: Helpers

    private func span(_ id: String) -> Totals? {
        model.vm?.devices.first { $0.deviceId == id }?.span
    }

    private func name(_ id: String) -> String {
        if id == model.peers?.device.deviceId { return model.peers?.device.displayName ?? String(localized: "This Mac") }
        return model.peers?.peers.first { $0.deviceId == id }?.name ?? String(id.prefix(8))
    }
}

/// Sheet target (Identifiable for `.sheet(item:)`).
private struct PairTarget: Identifiable {
    var address: String
    var id: String { address }
}

private struct PairedDeviceRow: View {
    @Environment(AppModel.self) private var model
    let device: PeerDevice
    let span: Totals?
    let forget: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: device.lastErrorCode == nil ? "desktopcomputer" : "exclamationmark.triangle.fill")
                .foregroundStyle(device.lastErrorCode == nil ? Color.accentColor : .orange)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(device.name).fontWeight(.semibold)
                    Pill(text: device.lastAddress, tint: .secondary)
                }
                status.scaledFont(.caption).foregroundStyle(.secondary)
                if let s = span, device.enabled {
                    Text("\(Text(model.vm?.range.title ?? RangeSpec.week.title)) · \(Fmt.tokensExact(s.tokens)) tokens · \(Fmt.usd(s.costUsd))")
                        .scaledFont(.caption).foregroundStyle(.secondary).monospacedDigit()
                }
            }
            Spacer()
            Toggle("Include", isOn: Binding(
                get: { device.enabled },
                set: { model.setDeviceEnabled(device.deviceId, $0) }
            ))
            .toggleStyle(.switch).controlSize(.small).labelsHidden()
            .help("Include this device's usage in the statistics")
            Menu {
                Button("Forget Device…", role: .destructive, action: forget)
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
        }
        .padding(.vertical, 2)
    }

    @ViewBuilder private var status: some View {
        if let code = device.lastErrorCode {
            Text(AppModel.peerMessage(code: code)).foregroundStyle(.orange)
        } else if let seen = device.lastSeenAt {
            Text("Synced \(Fmt.relative(seen))")
        } else {
            Text("Not synced yet")
        }
    }
}

private struct NearbyRow: View {
    let device: FoundDevice
    let pair: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: "dot.radiowaves.left.and.right").foregroundStyle(.secondary).frame(width: 18)
            VStack(alignment: .leading, spacing: 3) {
                Text(device.name).fontWeight(.semibold)
                Text(device.addresses.first ?? "—").scaledFont(.caption).foregroundStyle(.secondary)
                    .textSelection(.enabled)
                if device.warnings.contains("fingerprint_mismatch") {
                    Text("Its identity differs from the paired device with this ID — pulls will be refused.")
                        .scaledFont(.caption).foregroundStyle(.orange)
                } else if device.warnings.contains("duplicate_device_id") {
                    Text("Another device uses the same ID (a copied ledger?).")
                        .scaledFont(.caption).foregroundStyle(.orange)
                }
            }
            Spacer()
            if device.paired {
                Pill(text: String(localized: "Paired"), tint: .green)
            } else {
                Button("Pair…", action: pair).disabled(device.addresses.isEmpty)
            }
        }
        .padding(.vertical, 2)
    }
}

/// Address + one-time code. The code is proven digit by digit in core, so a
/// wrong code or an interceptor ends the attempt; the user then asks the
/// other device for a new code.
struct AddDeviceSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @ViewState private var address: String
    @ViewState private var code = ""
    @ViewState private var busy = false
    @ViewState private var error: String?

    init(initialAddress: String) {
        _address = ViewState(wrappedValue: initialAddress)
    }

    private var digits: String { code.filter(\.isNumber) }
    private var ready: Bool { !address.trimmingCharacters(in: .whitespaces).isEmpty && digits.count == 6 }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Add Device").scaledFont(.title2, weight: .semibold)
            Text("On the other device, start sharing. It shows a six-digit code that is valid for two minutes and works once.")
                .foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            Form {
                TextField("Address", text: $address, prompt: Text(verbatim: "192.168.1.20:4319"))
                TextField("Pairing Code", text: $code, prompt: Text(verbatim: "123456"))
                    .monospaced()
                    .onChange(of: code) { _, c in
                        let d = String(c.filter(\.isNumber).prefix(6))
                        if d != c { code = d }
                    }
            }
            .formStyle(.columns)
            .disabled(busy)
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                if busy {
                    ProgressView().controlSize(.small)
                    Text("Pairing…").foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Pair") { submit() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.glassProminent)
                    .disabled(!ready || busy)
            }
        }
        .padding(20)
        .frame(width: 420)
    }

    private func submit() {
        busy = true
        error = nil
        let (addr, c) = (address.trimmingCharacters(in: .whitespaces), digits)
        Task {
            let failure = await model.pair(address: addr, code: c)
            busy = false
            if let failure {
                error = failure
                code = ""
            } else {
                dismiss()
            }
        }
    }
}
