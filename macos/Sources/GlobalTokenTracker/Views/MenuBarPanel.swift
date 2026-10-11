// Menu bar extra — the Mac replacement for the Windows tray icon + tooltip.
// The label is the only always-visible UI, so it reads one cheap derived
// value and re-renders only when today's cost actually changes.
import SwiftUI

struct MenuBarLabel: View {
    let model: AppModel
    @AppStorage("menuBarShowsCost") private var showsCost = true

    var body: some View {
        if showsCost, let cost = model.vm?.today.costUsd {
            Label(Fmt.usd(cost), systemImage: "chart.bar.xaxis")
                .labelStyle(.titleAndIcon)
                .monospacedDigit()
        } else {
            Image(systemName: "chart.bar.xaxis")
        }
    }
}

struct MenuBarPanel: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Text("Today").font(.headline)
                Spacer()
                if model.isScanning {
                    ProgressView().controlSize(.small)
                } else if let at = model.lastScanAt {
                    Text(at, format: .relative(presentation: .named))
                        .font(.caption).foregroundStyle(.secondary)
                }
            }

            if let t = model.vm?.today {
                Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 8) {
                    GridRow {
                        MenuMetric("Cost", Fmt.usd(t.costUsd))
                        MenuMetric("Tokens", Fmt.tokensCompact(t.tokens))
                    }
                    GridRow {
                        MenuMetric("Requests", Fmt.count(t.events))
                        MenuMetric("Active", Fmt.duration(ms: t.activeMs))
                    }
                }
            } else {
                ProgressView().frame(maxWidth: .infinity)
            }

            if let worst = model.vm?.quotas.max(by: { ($0.usedPercent ?? 0) < ($1.usedPercent ?? 0) }),
               worst.usedPercent != nil {
                Divider()
                QuotaMeter(row: worst)
            }

            Divider()

            GlassEffectContainer(spacing: 8) {
                HStack(spacing: 8) {
                    Button {
                        openWindow(id: "main")
                        NSApp.activate()
                    } label: {
                        Label("Open", systemImage: "macwindow")
                    }
                    Button {
                        model.manualRefresh()
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                    .disabled(model.isScanning)
                    Spacer()
                    Button {
                        openSettings()
                        NSApp.activate()
                    } label: {
                        Image(systemName: "gearshape")
                    }
                    .help("Settings…")
                    Button {
                        NSApp.terminate(nil)
                    } label: {
                        Image(systemName: "power")
                    }
                    .help("Quit GlobalTokenTracker")
                }
                .buttonStyle(.glass)
                .controlSize(.regular)
            }
        }
        .padding(16)
        .frame(width: 300)
    }
}

private struct MenuMetric: View {
    let title: LocalizedStringKey
    let value: String

    init(_ title: LocalizedStringKey, _ value: String) {
        self.title = title
        self.value = value
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            Text(value)
                .font(.system(.title3, design: .rounded, weight: .semibold))
                .monospacedDigit()
                .contentTransition(.numericText())
        }
        .frame(minWidth: 110, alignment: .leading)
    }
}
