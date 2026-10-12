// Devices — LAN multi-device viewing (core::peer). This Mac and its share
// (on/off, pairing code, who can see it), the devices it has paired with
// (status, last sync, enable / forget), possible double counting, and
// devices found nearby (Bonjour plus a sweep of this subnet). Pairing is
// mutual: either side entering the other's code pairs both directions. Pairing and discovery run
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
            if !activeViewers.isEmpty {
                viewersSection.cascade(1)
            }
            pairedSection.cascade(1)
            if let warnings = model.peers?.warnings, !warnings.isEmpty {
                warningsSection(warnings).cascade(2)
            }
            nearbySection.cascade(3)
        }
        .formStyle(.grouped)
        .task { await model.loadPeers() }
        // While this page is in front and sharing, follow the code (used by
        // a pairing, expired) and newly paired viewers closely.
        .task(id: model.page == .devices && model.isSharing) {
            guard model.page == .devices, model.isSharing else { return }
            while !Task.isCancelled {
                await model.refreshShare()
                try? await Task.sleep(for: .seconds(2))
            }
        }
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
        Section {
            LabeledContent("Name", value: model.peers?.device.displayName ?? "—")
            LabeledContent("Device ID") {
                Text(model.peers.map { String($0.device.deviceId.prefix(8)) } ?? "—")
                    .monospaced().textSelection(.enabled)
            }
            Toggle(isOn: Binding(get: { model.isSharing }, set: { model.setSharing($0) })) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Share This Mac")
                    Text("Paired devices on this network can read this Mac's hourly usage totals.")
                        .scaledFont(.caption).foregroundStyle(.secondary)
                }
            }
            .toggleStyle(.switch)
            .disabled(model.isSwitchingShare)
            if let status = model.share?.status {
                PairingCodeRow(status: status, expiry: model.codeExpiry) { model.newPairingCode() }
                    .transition(.opacity)
                LabeledContent("Address") {
                    // Every LAN address, home network first (VPN / proxy
                    // tunnels are left out by core).
                    VStack(alignment: .trailing, spacing: 2) {
                        if status.addresses.isEmpty {
                            Text("No network")
                        }
                        ForEach(status.addresses, id: \.self) { a in
                            Text(verbatim: a)
                        }
                    }
                    .monospaced().textSelection(.enabled)
                }
                .transition(.opacity)
            }
            if let err = model.shareError {
                Label(err, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange).scaledFont(.callout)
            }
        } header: {
            Text("This Mac")
        } footer: {
            Text("Pairing is mutual: when the other device enters this Mac's code — or this Mac enters the other's — both see each other's usage. Only hourly totals are shared, over an encrypted connection.")
                .scaledFont(.caption).foregroundStyle(.secondary)
        }
        .animation(Motion.resolved(Motion.select), value: model.isSharing)
    }

    private var activeViewers: [ShareViewer] {
        (model.share?.viewers ?? []).filter { $0.revokedAt == nil }
    }

    private var viewersSection: some View {
        Section {
            ForEach(activeViewers) { v in
                HStack(spacing: 10) {
                    Image(systemName: "eye").foregroundStyle(.secondary).frame(width: 18)
                    VStack(alignment: .leading, spacing: 3) {
                        Text(v.name).fontWeight(.semibold)
                        Group {
                            if let seen = v.lastSeenAt {
                                Text("Last read \(Fmt.relative(seen))")
                            } else {
                                Text("Paired \(Fmt.relative(v.createdAt)), not read yet")
                            }
                        }
                        .scaledFont(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Revoke", role: .destructive) { model.revokeViewer(v.id) }
                        .help("Its next request is refused; it has to pair again")
                }
                .padding(.vertical, 2)
            }
        } header: {
            Text("Devices That Can See This Mac")
        }
    }

    private var pairedSection: some View {
        Section {
            let peers = model.peers?.peers ?? []
            if peers.isEmpty {
                Text("No paired devices yet. Turn on sharing on either device to get a pairing code, then enter it on the other one — pairing works both ways.")
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
            Text("Finds devices that are sharing, over Bonjour and by checking this Mac's own network. Not listed? Guest Wi-Fi, VLANs and client isolation block it — add the device by its address instead, e.g. 192.168.1.20:4319.")
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

/// The code on offer, large and grouped (482 913), with its countdown.
private struct PairingCodeRow: View {
    let status: ShareStatus
    let expiry: Date?
    let newCode: () -> Void

    var body: some View {
        HStack(alignment: .center, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Pairing Code").scaledFont(.subheadline).foregroundStyle(.secondary)
                if let code = status.code {
                    Text(verbatim: Self.grouped(code))
                        .scaledFont(.title, weight: .semibold, design: .monospaced)
                        .textSelection(.enabled)
                        .contentTransition(.numericText())
                        .accessibilityLabel(code.map(String.init).joined(separator: " "))
                    TimelineView(.periodic(from: .now, by: 1)) { ctx in
                        let left = max(0, Int((expiry ?? ctx.date).timeIntervalSince(ctx.date).rounded()))
                        Text(left > 0
                             ? "Valid for \(left / 60):\(String(format: "%02d", left % 60)) · works once"
                             : "Expired")
                            .scaledFont(.caption).monospacedDigit()
                            .foregroundStyle(left > 0 ? Color.secondary : Color.orange)
                    }
                } else {
                    Text("No code on offer — it was used or has expired.")
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
            Button(status.code == nil ? "Create Code" : "New Code", action: newCode)
                .buttonStyle(.glass)
        }
        .padding(.vertical, 4)
        .animation(Motion.resolved(Motion.select), value: status.code)
    }

    static func grouped(_ code: String) -> String {
        code.count == 6 ? "\(code.prefix(3)) \(code.suffix(3))" : code
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
            Text("On the other device, turn on sharing. It shows a six-digit code that is valid for two minutes and works once. Pairing is mutual: afterwards both devices see each other's usage, and this Mac starts sharing too.")
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
