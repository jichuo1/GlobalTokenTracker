// SwiftUI shell over the shared Rust scheduling client. No provider credentials.
import SwiftUI
import Foundation

indirect enum SchedulerJSON: Codable, Sendable {
    case object([String: SchedulerJSON]), array([SchedulerJSON]), string(String), number(Double), bool(Bool), null
    init(from decoder: Decoder) throws {
        let box = try decoder.singleValueContainer()
        if box.decodeNil() { self = .null }
        else if let v = try? box.decode(Bool.self) { self = .bool(v) }
        else if let v = try? box.decode(Double.self) { self = .number(v) }
        else if let v = try? box.decode(String.self) { self = .string(v) }
        else if let v = try? box.decode([SchedulerJSON].self) { self = .array(v) }
        else { self = .object(try box.decode([String: SchedulerJSON].self)) }
    }
    func encode(to encoder: Encoder) throws {
        var box = encoder.singleValueContainer()
        switch self {
        case .object(let v): try box.encode(v)
        case .array(let v): try box.encode(v)
        case .string(let v): try box.encode(v)
        case .number(let v): try box.encode(v)
        case .bool(let v): try box.encode(v)
        case .null: try box.encodeNil()
        }
    }
    subscript(_ key: String) -> SchedulerJSON {
        get { if case .object(let v) = self { return v[key] ?? .null }; return .null }
        set { if case .object(var v) = self { v[key] = newValue; self = .object(v) } }
    }
    var text: String { if case .string(let v) = self { return v }; return "" }
    var values: [SchedulerJSON] { if case .array(let v) = self { return v }; return [] }
    var fields: [String: SchedulerJSON] { if case .object(let v) = self { return v }; return [:] }
    var pretty: String { String(decoding: (try? JSONEncoder.pretty.encode(self)) ?? Data(), as: UTF8.self) }
    static func encode<T: Encodable>(_ value: T) throws -> SchedulerJSON {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        return try JSONDecoder().decode(Self.self, from: encoder.encode(value))
    }
    func decode<T: Decodable>(_ type: T.Type) throws -> T {
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(type, from: JSONEncoder().encode(self))
    }
}
private extension JSONEncoder {
    static var pretty: JSONEncoder { let e = JSONEncoder(); e.outputFormatting = [.prettyPrinted, .sortedKeys]; return e }
}

private struct ScheduleRoute: Decodable, Identifiable {
    var routeId: String
    var source: String
    var model: String
    var effort: String
    var connectivity: String
    var lastError: String?
    var id: String { routeId }
}
private struct SchedulePool: Codable, Identifiable {
    var routeId: String
    var expiresAt: UInt64
    var tags: [String] = []
    var id: String { routeId }
}
private struct ScheduleNode: Codable, Identifiable {
    var id: String
    var title: String
    var routeId: String
    var nodeType = "model"
    var role = ""
    var fallbackRoutes: [String] = []
    var tags: [String] = []
    var x: Double
    var y: Double
    var task: SchedulerJSON
    var usesModel: Bool { ["model", "operator", "reviewer"].contains(nodeType) }
}
private enum ScheduleEdgeKind: String, Codable { case task, report }
private enum ScheduleConnectionMode: String, CaseIterable, Identifiable {
    case task, report, bidirectional
    var id: String { rawValue }
    var title: String { switch self {case .task: return "任务线";case .report: return "回报线";case .bidirectional: return "任务 + 回报"} }
    var kind: ScheduleEdgeKind { self == .report ? .report : .task }
}
private struct ScheduleEdge: Codable {
    var from: String
    var to: String
    var kind: ScheduleEdgeKind = .task
    init(from: String, to: String, kind: ScheduleEdgeKind = .task) { self.from = from; self.to = to; self.kind = kind }
    private enum CodingKeys: String, CodingKey { case from, to, kind }
    init(from decoder: Decoder) throws {
        let box = try decoder.container(keyedBy: CodingKeys.self)
        from = try box.decode(String.self, forKey: .from); to = try box.decode(String.self, forKey: .to)
        kind = try box.decodeIfPresent(ScheduleEdgeKind.self, forKey: .kind) ?? .task
    }
    func encode(to encoder: Encoder) throws {
        var box = encoder.container(keyedBy: CodingKeys.self)
        try box.encode(from, forKey: .from); try box.encode(to, forKey: .to)
        if kind != .task { try box.encode(kind, forKey: .kind) }
    }
}
private struct ScheduleGraph: Codable, Identifiable {
    var id = UUID().uuidString
    var name = "定向工作流"
    var nodes: [ScheduleNode] = []
    var edges: [ScheduleEdge] = []
    var tokenBudget: UInt64 = 200000
    var maxParallel = 2
    var lunaSupervision = true
    var maxRounds = 8
    init() {}
    private enum CodingKeys: String, CodingKey { case id, name, nodes, edges, tokenBudget, maxParallel, lunaSupervision, maxRounds }
    init(from decoder: Decoder) throws {
        let box = try decoder.container(keyedBy: CodingKeys.self)
        id = try box.decode(String.self, forKey: .id); name = try box.decode(String.self, forKey: .name)
        nodes = try box.decode([ScheduleNode].self, forKey: .nodes); edges = try box.decodeIfPresent([ScheduleEdge].self, forKey: .edges) ?? []
        tokenBudget = try box.decodeIfPresent(UInt64.self, forKey: .tokenBudget) ?? 200000
        maxParallel = try box.decodeIfPresent(Int.self, forKey: .maxParallel) ?? 2
        lunaSupervision = try box.decodeIfPresent(Bool.self, forKey: .lunaSupervision) ?? true
        maxRounds = try box.decodeIfPresent(Int.self, forKey: .maxRounds) ?? 8
    }
    func encode(to encoder: Encoder) throws {
        var box = encoder.container(keyedBy: CodingKeys.self)
        try box.encode(id, forKey: .id); try box.encode(name, forKey: .name)
        try box.encode(nodes, forKey: .nodes); try box.encode(edges, forKey: .edges)
        try box.encode(tokenBudget, forKey: .tokenBudget); try box.encode(maxParallel, forKey: .maxParallel)
        try box.encode(lunaSupervision, forKey: .lunaSupervision)
        if maxRounds != 8 { try box.encode(maxRounds, forKey: .maxRounds) }
    }
    mutating func chainTasks() {
        let codex = nodes.filter { $0.nodeType == "codex" }
        let inputs = nodes.filter { $0.nodeType == "input" }
        let head = codex.count == 1 ? codex.first : (codex.isEmpty && inputs.count == 1 ? inputs.first : nil)
        var ordered = nodes.filter { !["codex", "input"].contains($0.nodeType) }
        if let head { ordered.insert(head, at: 0) }
        edges = edges.filter { $0.kind == .report } + zip(ordered, ordered.dropFirst()).map { ScheduleEdge(from: $0.0.id, to: $0.1.id) }
    }
}
private struct ScheduleTemplate: Decodable, Identifiable {
    var id: String
    var title: String
    var nodeType: String
    var role: String
    var tags: [String]
    var task: SchedulerJSON
}
private struct ScheduleSettings: Codable {
    var revision: UInt64 = 0
    var enabled = true
    var mode = "auto"
    var sources: [String: Bool] = [:]
    var pool: [SchedulePool] = []
    var workflows: [ScheduleGraph] = []
    var callProfile: SchedulerJSON = .object(["preset": .string("inherit")])
}

struct SchedulingView: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ViewState private var settings = ScheduleSettings()
    @ViewState private var routes: [ScheduleRoute] = []
    @ViewState private var sources: [String] = []
    @ViewState private var runs: [SchedulerJSON] = []
    @ViewState private var selected = 0
    @ViewState private var nodeId: String?
    @ViewState private var from: String?
    @ViewState private var connectionMode: ScheduleConnectionMode = .task
    @ViewState private var fromMode: ScheduleConnectionMode = .task
    @ViewState private var query = ""
    @ViewState private var minutes = 120
    @ViewState private var busy = false
    @ViewState private var dirty = false
    @ViewState private var error: String?
    @ViewState private var notice = ""
    @ViewState private var workflowId = ""
    @ViewState private var input = ""
    @ViewState private var executionRoot = ""
    @ViewState private var writablePaths = ""
    @ViewState private var runId = ""
    @ViewState private var detail = ""
    @ViewState private var zoom = 1.0
    @ViewState private var origin: CGPoint?
    @ViewState private var pan = CGPoint(x: 24, y: 24)
    @ViewState private var panOrigin: CGPoint?
    @ViewState private var viewport = CGSize(width: 800, height: 500)
    @ViewState private var panTool = false
    @ViewState private var showInspector = false
    @ViewState private var workflowSettings = false
    @ViewState private var pickedRoute: String?
    @ViewState private var draggedNode: String?
    @ViewState private var dropTarget: String?
    @ViewState private var canvasTargeted = false
    @ViewState private var hoverPoint: CGPoint?
    @ViewState private var supervisorWorkflows: [String] = []
    @ViewState private var showRun = false
    @ViewState private var showLibrary = true
    @ViewState private var templates: [ScheduleTemplate] = []
    @ViewState private var probeSupervision = true
    @ViewState private var taskText = ""
    @ViewState private var profileText = ""
    @ViewState private var callPresets: [SchedulerJSON] = []
    @ViewState private var effectiveProfile = ""
    @ViewState private var tab = "workflow"
    @ViewState private var submitKey = ""
    @ViewState private var submitSignature = ""
    @ViewState private var finished = false
    @ViewState private var acknowledge = false
    @ViewState private var detailValue: SchedulerJSON = .null
    @ViewState private var report = ""

    private var graph: ScheduleGraph? { settings.workflows.indices.contains(selected) ? settings.workflows[selected] : nil }
    private var nodeIndex: Int? { graph?.nodes.firstIndex { $0.id == nodeId } }
    private var filtered: [ScheduleRoute] { routes.filter { query.isEmpty || "\($0.source) \($0.model)".localizedCaseInsensitiveContains(query) } }
    private var now: UInt64 { UInt64(Date().timeIntervalSince1970 * 1000) }
    private var hasOperators: Bool { graph?.nodes.contains { $0.nodeType == "operator" } ?? false }
    private var operatorPaths: [String] {
        writablePaths.split(whereSeparator: { $0.isNewline }).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
    }
    private var operatorAccessError: String? {
        guard hasOperators else { return nil }
        let root = executionRoot.trimmingCharacters(in: .whitespacesAndNewlines)
        if !root.hasPrefix("/") { return "操作员需要本次授权的绝对工作区目录。" }
        if operatorPaths.isEmpty { return "请逐行指定本次允许操作员修改的精确文件路径。" }
        if operatorPaths.contains(where: { path in
            path.hasPrefix("/") || path.contains("\\") || path.hasSuffix("/") || path.contains("*") || path.contains("?") ||
                path.split(separator: "/", omittingEmptySubsequences: false).contains(where: { $0.isEmpty || $0 == "." || $0 == ".." })
        }) { return "可写路径须为精确的 POSIX 相对文件名，不接受目录、通配符或上级路径。" }
        return nil
    }
    private var executionGrant: SchedulerJSON? {
        guard hasOperators else { return nil }
        return .object(["root": .string(executionRoot.trimmingCharacters(in: .whitespacesAndNewlines)),
                        "writable_paths": .array(operatorPaths.map { .string($0) })])
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            header
            if let error {
                Label { Text(error).lineLimit(3).textSelection(.enabled) } icon: {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.red)
                }
                .font(.callout)
                .padding(.horizontal, 10).padding(.vertical, 6)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
            }
            if !notice.isEmpty {
                Label { Text(notice).lineLimit(4).textSelection(.enabled) } icon: { Image(systemName: "info.circle") }
                    .font(.callout).foregroundStyle(.secondary)
            }
            switch tab {
            case "sources": sourcePanel
            case "profiles": profilePanel
            default: workspace
            }
        }.padding(16).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .task { request() }
        .task(id: runId) {
            let id=runId; guard !id.isEmpty else {return}
            while !Task.isCancelled {
                try? await Task.sleep(for:.seconds(1))
                if busy {continue}
                guard running else {break}
                poll(id)
            }
        }
        .onChange(of: nodeId) { _, _ in taskText = nodeIndex.flatMap { graph?.nodes[$0].task.pretty } ?? "" }
        .onChange(of: connectionMode) { _, _ in from = nil }
    }

    /// Page switcher on the left; sync state, 保存修改 and 更多 on the right —
    /// the same spot on all three pages (the WinUI band mirrors this). 保存
    /// is the prominent button only while there is something to save.
    private var header: some View {
        HStack(alignment: .center, spacing: 10) {
            Picker("调度视图", selection: $tab) {
                Text("工作流画布").tag("workflow"); Text("来源与模型池").tag("sources"); Text("调用配置").tag("profiles")
            }.pickerStyle(.segmented).labelsHidden().fixedSize()
            Spacer(minLength: 12)
            if busy { ProgressView().controlSize(.small) }
            Text(busy ? "处理中…" : dirty ? "有未保存修改" : "已同步")
                .font(.callout).foregroundStyle(dirty ? Color.orange : Color.secondary)
            saveButton
            moreMenu
        }
    }
    @ViewBuilder private var saveButton: some View {
        let button = Button("保存修改") { save() }
            .disabled(busy || !dirty)
            .keyboardShortcut("s", modifiers: .command)
            .help("保存节点和配置（⌘S）；保存本身不会调用模型。")
        if dirty { button.buttonStyle(.borderedProminent) } else { button }
    }
    private var running: Bool { ["running", "cancelling", "awaiting_codex"].contains(detailValue["state"].text) }
    private var codexWaiting: Bool {
        detailValue["state"].text == "awaiting_codex" || detailValue["nodes"].fields.values.contains { $0["state"].text == "awaiting_codex" }
    }
    private func runtimeStateText(_ state: String) -> String {
        switch state {
        case "awaiting_plan": return "待母节点规划"
        case "awaiting_assignment": return "待分片分配"
        case "executing": return "操作执行中"
        case "awaiting_codex": return "待Codex确认"
        default: return state
        }
    }
    private func connectivityLabel(_ value: String) -> String {
        switch value {
        case "verified": return "已连通"
        case "", "unverified": return "未验证"
        case "verification_required": return "需要验证"
        default: return "连通异常"
        }
    }

    /// Grouped form like Settings and Devices: every switch and button sits on
    /// the trailing edge of its row, so the columns line up without manual
    /// spacers (the old loose HStacks put each control wherever its label ended).
    private var sourcePanel: some View {
        Form {
            Section {
                Toggle("允许模型调度", isOn: Binding(get: {settings.enabled}, set: {settings.enabled=$0;dirty=true}))
                Toggle("定向模式 · 仅使用限时池", isOn: Binding(get: {settings.mode=="directed"}, set: {settings.mode=$0 ? "directed":"auto";dirty=true}))
                Toggle("手动检测启用 Luna", isOn: $probeSupervision)
            } header: { Text("调度开关") } footer: {
                Text("自动模式按能力选路；定向模式按池内顺序选路，池到期后停止派发。").foregroundStyle(.secondary)
            }
            Section("来源") {
                if sources.isEmpty { Text("尚未发现来源。请检查协调器是否已启动。").foregroundStyle(.secondary) }
                ForEach(sources, id: \.self) { source in
                    Toggle(isOn: Binding(get: { settings.sources[source] ?? true }, set: { settings.sources[source] = $0; dirty = true })) {
                        Text(source)
                        Text("\(routes.filter { $0.source == source }.count) 个模型 · \(routes.filter { $0.source == source && $0.connectivity == "verified" }.count) 个近期生成验证")
                    }
                }
            }
            Section {
                TextField("筛选", text: $query, prompt: Text("模型或来源名称"))
                Stepper("入池有效期：\(minutes) 分钟", value: $minutes, in: 1...10080)
                if filtered.isEmpty { Text("没有匹配的模型。").foregroundStyle(.secondary) }
                ForEach(filtered) { route in routeRow(route) }
            } header: { Text("模型与来源") } footer: {
                Text("扫描只读取模型目录；生成检测会产生少量用量，监工可选，并遵守共享探针预算。列表可见不代表已验证可用。").foregroundStyle(.secondary)
            }
            Section("限时模型池") {
                if settings.pool.isEmpty { Text("尚未加入任何模型。").foregroundStyle(.secondary) }
                ForEach(Array(settings.pool.enumerated()), id: \.element.id) { index, entry in
                    HStack(spacing: 10) {
                        Text("\(index + 1)").monospacedDigit().foregroundStyle(.secondary).frame(minWidth: 20, alignment: .trailing)
                        Text(routes.first { $0.id == entry.id }?.model ?? entry.id).lineLimit(1).truncationMode(.middle)
                        Spacer(minLength: 8)
                        Text(entry.expiresAt > now ? "剩余 \((entry.expiresAt - now) / 60000) 分钟" : "已到期")
                            .font(.caption).monospacedDigit().foregroundStyle(entry.expiresAt > now ? Color.secondary : Color.orange)
                        ControlGroup {
                            Button { movePool(index, -1) } label: { Image(systemName: "chevron.up") }.disabled(index == 0).help("上移")
                            Button { movePool(index, 1) } label: { Image(systemName: "chevron.down") }.disabled(index == settings.pool.count - 1).help("下移")
                        }.fixedSize()
                        Button { settings.pool.remove(at: index); dirty = true } label: { Image(systemName: "minus.circle") }
                            .buttonStyle(.borderless).help("移出模型池")
                    }
                }
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
    }
    /// Model on the first line, source · connectivity on the second; actions
    /// are borderless icon buttons on the trailing edge with their meaning in
    /// the tooltip (the old rows repeated two text buttons per model).
    private func routeRow(_ route: ScheduleRoute) -> some View {
        let pooled = settings.pool.contains { $0.id == route.id && $0.expiresAt > now }
        return HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text(route.model).lineLimit(1).truncationMode(.middle)
                Text("\(route.source) · \(connectivityLabel(route.connectivity))\(route.lastError.map { " · \($0)" } ?? "")")
                    .font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
            }.draggable(route.id)
            Spacer(minLength: 8)
            Button { pool(route.id) } label: { Image(systemName: pooled ? "arrow.clockwise.circle" : "plus.circle") }
                .buttonStyle(.borderless).help(pooled ? "续期：按当前有效期重新计时" : "加入限时模型池")
            Button { request(params: .object(["action": .string("check"), "route_id": .string(route.id), "workflow_id": .string(workflowId), "luna_supervision": .bool(graph?.lunaSupervision ?? probeSupervision)])) } label: { Image(systemName: "bolt.circle") }
                .buttonStyle(.borderless).disabled(busy).help("生成检测：会产生少量用量")
        }
    }

    private var profilePanel: some View {
        Form {
            Section {
                LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: 8), count: 3), spacing: 8) {
                    ForEach(callPresets.indices, id: \.self) { i in
                        Button { settings.callProfile = callPresets[i]["value"]; profileText = settings.callProfile.pretty; dirty = true } label: {
                            Text(callPresets[i]["title"].text).frame(maxWidth: .infinity)
                        }.buttonStyle(.bordered)
                    }
                }
                if callPresets.isEmpty { Text("协调器未提供调用预设。").foregroundStyle(.secondary) }
            } header: { Text("预设") } footer: {
                Text("节点显式参数优先于预设默认值；配置档上限与模型原生上限共同生效。").foregroundStyle(.secondary)
            }
            Section("调用配置 JSON") {
                TextEditor(text: $profileText).font(.system(.body, design: .monospaced)).frame(minHeight: 180)
                HStack {
                    Spacer()
                    Button("应用到待保存设置") { do { let value = try JSONDecoder().decode(SchedulerJSON.self, from: Data(profileText.utf8)); guard case .object = value else { return }; settings.callProfile = value; dirty = true } catch { self.error = error.localizedDescription } }
                }
            }
            Section("当前生效值") {
                Text(effectiveProfile).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .formStyle(.grouped)
        .scrollContentBackground(.hidden)
    }

    private var workspace: some View {
        VStack(alignment: .leading, spacing: 10) {
            // Editing on the left, execution on the right; the canvas summary
            // sits between them so it no longer floats on its own line.
            HStack(spacing: 10) {
                workflowPicker
                addMenu
                viewMenu
                Picker("连线", selection: $connectionMode) {
                    ForEach(ScheduleConnectionMode.allCases) { mode in Text(mode.title).tag(mode) }
                }.fixedSize().help("任务线分配工作；回报线传递进度和结果；任务 + 回报一次建立双向通道。")
                Spacer(minLength: 12)
                Text(verbatim: "\(graph?.nodes.count ?? 0) 个节点 · 缩放 \(Int((zoom * 100).rounded()))%")
                    .font(.callout).monospacedDigit().foregroundStyle(.secondary)
                executionButtons
            }
            HSplitView {
                if showLibrary { modelLibrary }
                graphCanvas.frame(minWidth: 200, minHeight: 180)
                if showInspector { inspectorPanel }
            }.frame(maxWidth: .infinity, maxHeight: .infinity)
            statusBar
            if showRun { runPanel.frame(height: 230) }
        }
    }
    @ViewBuilder private var statusBar: some View {
        if let i=nodeIndex,let node=graph?.nodes[i] {
            HStack(alignment:.center,spacing:12) {
                VStack(alignment:.leading,spacing:2) {
                    Text("已选中：\(node.title)").fontWeight(.semibold).foregroundStyle(Color.accentColor).lineLimit(1)
                    Text(connectionMode == .report ? "拖动主体移动 · 左端口发起回报，右端口接收回报" : "拖动主体移动 · 右端口发起任务，左端口接收任务").font(.caption).foregroundStyle(.secondary)
                }
                Spacer(minLength: 12)
                Button("编辑节点") {workflowSettings=false;showInspector=true}.help("查看当前节点的职责、提示词和参数。")
                Button("取消选择") {nodeId=nil;showInspector=false}
            }
        } else if let reason = runReason {
            Label(reason, systemImage: "info.circle").font(.callout).foregroundStyle(.secondary)
        } else {
            Label("就绪：可以运行工作流（⌘↩）。", systemImage: "checkmark.circle").font(.callout).foregroundStyle(.secondary)
        }
    }
    private var runReason: String? {
        if busy {return "正在处理请求，请稍候。"}
        if running {return "已有运行进行中，可查看结果或停止。"}
        guard let g=graph,!g.nodes.isEmpty else {return "先从模型库拖入模型，或通过添加节点选择职责模板。"}
        if dirty {return "请先保存修改，再运行工作流。"}
        if !settings.enabled {return "调度总开关已关闭，请在来源与模型池中检查。"}
        if let reason = operatorAccessError { return reason }
        if g.lunaSupervision && !supervisorWorkflows.contains(workflowId) {return "需要同一工作流的在线 Luna；可在工作流设置中关闭监工。"}
        return nil
    }
    private var moreMenu: some View {
        Menu {
            Section("运行准备") {
                Button("运行前预检", systemImage: "checklist") {preflight()}.disabled(busy || graph==nil).help("只计算路线和预算，不生成模型内容。")
                Button("工作流设置…", systemImage: "slider.horizontal.3") {tab="workflow";workflowSettings=true;showInspector=true}.help("调整预算、并发和可选 Luna 监工。")
            }
            Section("服务管理") {
                Button("刷新状态", systemImage: "arrow.clockwise") {request()}.disabled(busy)
                Button("重新扫描来源", systemImage: "arrow.triangle.2.circlepath") {request(params:.object(["action":.string("refresh")]))}.disabled(busy).help("读取目录；不会发出生成检测。")
                Button("启动协调器服务", systemImage: "power") {request("router_start")}.disabled(busy)
            }
            Divider()
            Button("放弃修改并重载", systemImage: "arrow.uturn.backward", role: .destructive) {dirty=false;nodeId=nil;showInspector=false;request()}.disabled(busy || !dirty).help("放弃未保存修改，读取已有设置。")
        } label: { Label("更多", systemImage: "ellipsis.circle") }
        .fixedSize()
    }
    private func preflight() {showRun=true;tab="workflow";guard let graph else {return};do {request("router_workflow_plan",params:.object(["definition":try SchedulerJSON.encode(graph),"workflow_id":.string(workflowId)]))} catch {self.error=error.localizedDescription}}
    private var workflowPicker: some View {
        Picker("工作流", selection: $selected) {
            ForEach(Array(settings.workflows.enumerated()), id: \.element.id) { i,g in Text(g.name).tag(i) }
        }.labelsHidden().frame(width: 220).onChange(of:selected) { _,_ in nodeId=nil; from=nil }
    }
    private var addMenu: some View {
        Menu {
            Button("母节点 · Codex 接口", systemImage: "cpu") {addCodex()}
                .help("等待真实 Codex 调用方规划、切片与验收；本机调度器不会自行启动 Codex。")
            Divider()
            ForEach(templates.filter { $0.nodeType != "codex" }) {t in Button(t.title) {addTemplate(t)}.help(t.role)}
        } label: { Label("添加节点", systemImage: "plus") }
        .fixedSize().help("选择职责模板，或直接从模型库拖入模型。")
    }
    private var viewMenu: some View {
        Menu {
            Button("适应全图", systemImage: "arrow.up.left.and.down.right.magnifyingglass") {fitCanvas()}.help("让全部节点进入当前视口。")
            Button("放大", systemImage: "plus.magnifyingglass") {zoomCanvas(0.1)}.disabled(zoom >= 1.8)
            Button("缩小", systemImage: "minus.magnifyingglass") {zoomCanvas(-0.1)}.disabled(zoom <= 0.02)
            Button("实际大小", systemImage: "1.magnifyingglass") {zoomCanvas(1-zoom)}.disabled(abs(zoom-1) < 0.001)
            Divider()
            Toggle("平移工具", systemImage: "hand.raised", isOn: $panTool)
            Toggle("显示模型库", systemImage: "sidebar.left", isOn: $showLibrary)
            Toggle("显示输入与结果", systemImage: "rectangle.bottomthird.inset.filled", isOn: $showRun)
            Button("取消当前连线", systemImage: "xmark") {from=nil}.disabled(from == nil)
            Divider()
            Section("操作说明") {
                Text("空白处拖动平移；拖节点主体移动。")
                Text("任务：右端口 → 左端口；回报：左端口 → 右端口。")
                Text("任务线为强调色；回报线为橙色，并独立校验方向。")
                Text("任务 + 回报一次建立正向任务与反向回报，校验失败时不修改图。")
                Text("蓝色外框和底部名称表示已选中。")
            }
        } label: { Label("视图", systemImage: "eye") }
        .fixedSize().help("画布缩放、平移、面板显示和操作说明。")
    }
    private var supervisorToggle: some View {
        Toggle("Luna 监工", isOn: Binding(get: {graph?.lunaSupervision ?? probeSupervision}, set: {v in probeSupervision=v;mutate {$0.lunaSupervision=v}})).toggleStyle(.switch)
    }
    /// 运行 is prominent unless there are unsaved edits — then 保存 in the
    /// header carries it, so the page never shows two prominent buttons.
    private var executionButtons: some View {
        HStack(spacing: 8) {
            if running {Button("停止", systemImage: "stop.fill") {request("router_workflow_cancel",params:.object(["run_id":.string(runId)]))}.disabled(busy).help("请求停止；保留已发生的用量和结果。")}
            if dirty { runButton.buttonStyle(.bordered) } else { runButton.buttonStyle(.borderedProminent) }
        }
    }
    private var runButton: some View {
        Button {run()} label: {Label("运行工作流",systemImage:"play.fill")}
            .disabled(runReason != nil)
            .keyboardShortcut(.return, modifiers: .command)
            .help(runReason ?? "按已保存的节点和依赖调用模型，会占用预算（⌘↩）。")
    }
    private var modelLibrary: some View {
        VStack(alignment:.leading,spacing:8) {
            HStack(alignment:.firstTextBaseline) {
                Text("模型库").font(.headline)
                Spacer()
                Text("\(filtered.count)").font(.caption).monospacedDigit().foregroundStyle(.secondary)
            }
            TextField("搜索模型",text:$query,prompt:Text("模型或来源")).textFieldStyle(.roundedBorder)
            Text("拖入空白处创建节点；拖到模型节点上替换绑定。").font(.caption).foregroundStyle(.secondary).fixedSize(horizontal:false,vertical:true)
            // Two fixed lines per row (model, then source) so every row has the
            // same height; "source / model" wrapped only when long.
            ScrollView {LazyVStack(alignment:.leading,spacing:2) {ForEach(filtered) {r in
                VStack(alignment:.leading,spacing:1) {
                    Text(r.model).lineLimit(1).truncationMode(.middle)
                    Text(r.source).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }.padding(.horizontal,8).padding(.vertical,5).frame(maxWidth:.infinity,alignment:.leading)
                    .background(pickedRoute==r.id ? Color.accentColor.opacity(0.16):Color.clear,in:RoundedRectangle(cornerRadius:6))
                    .contentShape(Rectangle())
                    .onTapGesture {pickedRoute=r.id}
                    .draggable(r.id) {VStack(alignment:.leading,spacing:6) {Label(r.model,systemImage:"cpu").font(.headline);Text("松开放置模型节点").font(.caption)}.padding(14).frame(width:220).background(.regularMaterial,in:RoundedRectangle(cornerRadius:10)).overlay(RoundedRectangle(cornerRadius:10).stroke(Color.accentColor,lineWidth:2))}
                    .help("\(r.source) · \(connectivityLabel(r.connectivity))；拖入仅编排，运行后才调用模型。")
            }}}
        }.padding(10).frame(minWidth:180,idealWidth:210,maxWidth:260)
    }
    /// Title row with a close button, then a grouped form: labels on the
    /// leading edge, fields and switches on the trailing edge, one rhythm for
    /// workflow settings and node settings alike.
    private var inspectorPanel: some View {
        VStack(alignment:.leading,spacing:0) {
            HStack(alignment:.firstTextBaseline) {
                VStack(alignment:.leading,spacing:2) {
                    Text(workflowSettings ? "工作流设置":"节点设置").font(.headline)
                    Text(workflowSettings ? "配置本工作流的执行范围和保护条件。":"修改当前节点的职责与交付要求。").font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                Button {showInspector=false} label: {Image(systemName:"xmark")}.buttonStyle(.borderless).help("收起")
            }.padding(.horizontal,14).padding(.top,12).padding(.bottom,2)
            Form {
                if workflowSettings {workflowForm}
                else if let i=nodeIndex,let n=graph?.nodes[i] {editor(i,n)}
                else {Text("在画布上选中一个节点。").foregroundStyle(.secondary)}
            }
            .formStyle(.grouped)
            .scrollContentBackground(.hidden)
        }.frame(minWidth:280,idealWidth:320,maxWidth:380)
    }
    @ViewBuilder private var workflowForm: some View {
        if graph != nil {
            Section("基本") {
                TextField("名称",text:graphBinding(\.name))
                TextField("总 Token 预算",value:graphBinding(\.tokenBudget),format:.number)
                Stepper("并发目标：\(graph?.maxParallel ?? 2)",value:graphBinding(\.maxParallel),in:1...32)
                Stepper("最多调度轮次：\(graph?.maxRounds ?? 8)",value:graphBinding(\.maxRounds),in:1...64)
            }
        }
        Section {
            supervisorToggle
        } header: {Text("监工")} footer: {
            Text("开启时需要同一工作流的在线监工；关闭仍保留预算、模型池和恢复保护。").foregroundStyle(.secondary)
        }
        Section("工作流管理") {
            Button("新建工作流") {settings.workflows.append(ScheduleGraph());selected=settings.workflows.count-1;nodeId=nil;dirty=true}
            if graph != nil {
                Button("按顺序串联任务") {mutate { $0.chainTasks() }}.help("只重建任务线，保留已有回报线；母节点不接收任务线。")
                Button("清空连线") {mutate {$0.edges=[]}}
                Button("移除工作流",role:.destructive) {if graph != nil {settings.workflows.remove(at:selected);selected=0;nodeId=nil;dirty=true}}
            }
        }
    }
    private var runPanel: some View {
        VStack(alignment:.leading,spacing:8) {
            HStack {
                Text(running ? "输入与结果 · 运行中":"输入与结果").font(.headline)
                Spacer()
                Button {showRun=false} label: {Label("收起",systemImage:"xmark")}.buttonStyle(.borderless)
            }
            ScrollView {
                VStack(alignment:.leading,spacing:10) {
                    Grid(alignment:.leadingFirstTextBaseline,horizontalSpacing:12,verticalSpacing:8) {
                        GridRow {
                            Text("工作流 ID").gridColumnAlignment(.trailing)
                            TextField("工作流 ID",text:$workflowId,prompt:Text("default")).labelsHidden().textFieldStyle(.roundedBorder).frame(maxWidth:260)
                        }
                        GridRow(alignment:.top) {
                            Text("本次输入材料")
                            TextEditor(text:$input).frame(minHeight:60).overlay(RoundedRectangle(cornerRadius:5).stroke(Color(nsColor:.separatorColor)))
                        }
                        if hasOperators {
                            GridRow {
                                Text("本次操作工作区")
                                TextField("绝对目录", text: $executionRoot, prompt: Text("/Users/name/project"))
                                    .labelsHidden().textFieldStyle(.roundedBorder)
                            }
                            GridRow(alignment: .top) {
                                Text("精确可写文件")
                                VStack(alignment: .leading, spacing: 4) {
                                    TextEditor(text: $writablePaths).font(.system(.caption, design: .monospaced)).frame(minHeight: 60)
                                        .overlay(RoundedRectangle(cornerRadius: 5).stroke(Color(nsColor: .separatorColor)))
                                    Text("每行一个 POSIX 相对文件名，例如 src/main.rs；仅授权本次运行，不保存到模板，也不授权整个仓库。")
                                        .font(.caption).foregroundStyle(.secondary)
                                }
                            }
                        }
                        GridRow {
                            Text("付费风险")
                            Toggle("新建未知结果的运行前，接受重复付费风险",isOn:$acknowledge)
                        }
                    }
                    if !runs.isEmpty {
                        Text("最近运行").font(.subheadline.weight(.semibold))
                        LazyVGrid(columns:[GridItem(.adaptive(minimum:200),spacing:8)],alignment:.leading,spacing:8) {
                            ForEach(runs.indices,id:\.self) {i in Button("\(runs[i]["name"].text) · \(runtimeStateText(runs[i]["state"].text))") {runId=runs[i]["id"].text;poll(runId)}}
                        }
                    }
                    let jobs = detailValue["nodes"].fields.keys.sorted().filter {!detailValue["nodes"][$0]["job_id"].text.isEmpty}
                    if !jobs.isEmpty {
                        Text("节点报告").font(.subheadline.weight(.semibold))
                        LazyVGrid(columns:[GridItem(.adaptive(minimum:200),spacing:8)],alignment:.leading,spacing:8) {
                            ForEach(jobs,id:\.self) {id in Button("查看节点 \(id) 报告") {request("router_collect",params:.object(["job_id":detailValue["nodes"][id]["job_id"],"max_chars":.number(12000)]))}}
                        }
                    }
                    if codexWaiting {
                        Label("等待真实 Codex 调用方规划或验收回执；操作员修正须先获得母节点确认。", systemImage: "clock")
                            .font(.callout).foregroundStyle(.orange)
                    }
                    Text(report.isEmpty ? detail:report).font(.system(.caption,design:.monospaced)).textSelection(.enabled)
                }.frame(maxWidth:.infinity,alignment:.leading).multilineTextAlignment(.leading)
            }
        }
        .padding(12)
        .background(Color(nsColor:.controlBackgroundColor),in:RoundedRectangle(cornerRadius:10))
        .overlay(RoundedRectangle(cornerRadius:10).stroke(Color(nsColor:.separatorColor)))
    }
    private var graphCanvas: some View {
        GeometryReader { geometry in
            ZStack(alignment:.topLeading) {
                Color.secondary.opacity(0.06).contentShape(Rectangle()).gesture(DragGesture(minimumDistance:3).onChanged {v in
                    if panOrigin==nil {panOrigin=pan};guard let start=panOrigin else {return};pan=CGPoint(x:start.x+v.translation.width,y:start.y+v.translation.height)
                }.onEnded {_ in panOrigin=nil})
                // Routes are planned once per body evaluation; TimelineView only
                // re-runs the Canvas closure each frame to move the flow dots.
                let routed = routedEdges()
                TimelineView(.animation(paused: reduceMotion || routed.isEmpty)) { timeline in
                    Canvas {context,size in
                        let grid=Color.secondary.opacity(0.15)
                        for x in stride(from:pan.x.truncatingRemainder(dividingBy:24),to:size.width,by:24) {var p=Path();p.move(to:CGPoint(x:x,y:0));p.addLine(to:CGPoint(x:x,y:size.height));context.stroke(p,with:.color(grid),lineWidth:0.5)}
                        for y in stride(from:pan.y.truncatingRemainder(dividingBy:24),to:size.height,by:24) {var p=Path();p.move(to:CGPoint(x:0,y:y));p.addLine(to:CGPoint(x:size.width,y:y));context.stroke(p,with:.color(grid),lineWidth:0.5)}
                        let phase = reduceMotion ? 0 : CGFloat(timeline.date.timeIntervalSinceReferenceDate * 46)
                        for edge in routed { drawEdge(edge.kind, world: edge.path, context: context, phase: phase) }
                        if let from, let g = graph, let a = g.nodes.first(where: { $0.id == from }), let pointer = hoverPoint {
                            let cards = g.nodes.map { CGPoint(x: $0.x, y: $0.y) }
                            let z = CGFloat(zoom)
                            let world = CGPoint(x: (pointer.x - pan.x) / z, y: (pointer.y - pan.y) / z)
                            let kind = fromMode.kind
                            let tip = EdgeRouting.Port(at: world, out: kind == .report ? 1 : -1)
                            drawEdge(kind, world: EdgeRouting.route(from: port(a, kind: kind, source: true), to: tip, cards: cards), context: context, phase: nil)
                            if fromMode == .bidirectional {
                                let back = EdgeRouting.Port(at: CGPoint(x: world.x, y: world.y + 34), out: -1)
                                drawEdge(.report, world: EdgeRouting.route(from: back, to: port(a, kind: .report, source: false), cards: cards), context: context, phase: nil)
                            }
                        }
                    }
                }.allowsHitTesting(false)
                if let g=graph {ForEach(g.nodes) {n in nodeCard(n).scaleEffect(zoom,anchor:.topLeading).offset(x:pan.x+n.x*zoom,y:pan.y+n.y*zoom).allowsHitTesting(!panTool)}}
            }.frame(width:geometry.size.width,height:geometry.size.height).clipped()
            .overlay(alignment: .topTrailing) {
                HStack(spacing: 12) {
                    Label("任务 →", systemImage: "arrow.right").foregroundStyle(Color.accentColor)
                    Label("回报 →", systemImage: "arrow.turn.up.left").foregroundStyle(.orange)
                }.font(.caption).padding(8).background(.regularMaterial, in: RoundedRectangle(cornerRadius: 6)).padding(8)
            }
            .overlay(Rectangle().stroke(canvasTargeted ? Color.accentColor.opacity(0.6):Color.clear,lineWidth:2))
            .onContinuousHover {phase in switch phase {case .active(let point):hoverPoint=point;case .ended:hoverPoint=nil}}
            .onAppear {viewport=geometry.size}.onChange(of:geometry.size) {_,v in viewport=v}
            .dropDestination(for:String.self) {values,point in
                guard let id=values.first,let route=routes.first(where:{$0.id==id}) else {return false}
                if graph==nil {settings.workflows.append(ScheduleGraph());selected=settings.workflows.count-1}
                let x=min(10000,max(-10000,(point.x-pan.x)/zoom)),y=min(10000,max(-10000,(point.y-pan.y)/zoom))
                mutate {g in if g.nodes.count<256 {let id=UUID().uuidString;g.nodes.append(ScheduleNode(id:id,title:route.model,routeId:route.id,x:x,y:y,task:.object(["kind":.string("extract"),"prompt":.string("分析输入材料，返回完整 JSON 报告。"),"max_output_tokens":.number(8192),"token_budget":.number(48000),"deadline_s":.number(300),"hard_deadline_s":.number(900)])));nodeId=id}}
                pool(route.id);return true
            } isTargeted: {canvasTargeted=$0}
        }
    }
    private func edgePoint(_ node: ScheduleNode, kind: ScheduleEdgeKind, source: Bool) -> CGPoint {
        let right = kind == .report ? !source : source
        return CGPoint(x: pan.x + (node.x + (right ? 220 : 0)) * zoom,
                       y: pan.y + (node.y + (kind == .report ? 78 : 44)) * zoom)
    }
    /// Task lines leave the source's right port for the target's left port;
    /// report lines leave the left port for the target's right port. The
    /// route goes around every card (EdgeRouting, shared with Windows).
    private func port(_ node: ScheduleNode, kind: ScheduleEdgeKind, source: Bool) -> EdgeRouting.Port {
        let right = kind == .report ? !source : source
        return EdgeRouting.Port(at: CGPoint(x: node.x + (right ? 220 : 0), y: node.y + (kind == .report ? 78 : 44)), out: right ? 1 : -1)
    }
    private func routedEdges() -> [(kind: ScheduleEdgeKind, path: [CGPoint])] {
        guard let g = graph else { return [] }
        let cards = g.nodes.map { CGPoint(x: $0.x, y: $0.y) }
        let byId = Dictionary(g.nodes.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        return g.edges.compactMap { e in
            guard let a = byId[e.from], let b = byId[e.to] else { return nil }
            return (kind: e.kind, path: EdgeRouting.route(from: port(a, kind: e.kind, source: true), to: port(b, kind: e.kind, source: false), cards: cards))
        }
    }
    /// The rounded route in a softened tone, bright dots flowing toward the
    /// target on top, and a filled arrowhead on the final straight stub.
    /// `phase == nil` draws the dashed drag preview without dots.
    private func drawEdge(_ kind: ScheduleEdgeKind, world: [CGPoint], context: GraphicsContext, phase: CGFloat?) {
        guard world.count >= 2 else { return }
        let z = CGFloat(zoom)
        let toScreen = CGAffineTransform(a: z, b: 0, c: 0, d: z, tx: pan.x, ty: pan.y)
        let color: Color = kind == .report ? .orange : .accentColor
        let path = EdgeRouting.roundedPath(world).applying(toScreen)
        context.stroke(path, with: .color(color.opacity(phase == nil ? 0.9 : 0.65)),
                       style: StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round, dash: phase == nil ? [5, 4] : []))
        let line = EdgeRouting.flatten(world).map { $0.applying(toScreen) }
        if let phase {
            let r = min(3.2, max(1.8, 2.6 * z))
            for p in EdgeRouting.dots(line, spacing: 64 * z, phase: phase * z) {
                context.fill(Path(ellipseIn: CGRect(x: p.x - r, y: p.y - r, width: r * 2, height: r * 2)), with: .color(color))
            }
        }
        let tip = line[line.count - 1], tail = line[line.count - 2]
        let dx = tip.x - tail.x, dy = tip.y - tail.y
        let len = (dx * dx + dy * dy).squareRoot()
        guard len > 0.5 else { return }
        let ux = dx / len, uy = dy / len
        let size = min(10, max(6, 9 * z)), half = size / 2
        let bx = tip.x - ux * size, by = tip.y - uy * size
        var head = Path()
        head.move(to: tip)
        head.addLine(to: CGPoint(x: bx - uy * half, y: by + ux * half))
        head.addLine(to: CGPoint(x: bx + uy * half, y: by - ux * half))
        head.closeSubpath()
        context.fill(head, with: .color(color))
    }
    /// Right-click menu for a node, matching the Windows node menu: name and
    /// availability first, then the node's frequent actions. Native menus
    /// scroll on their own when they outgrow the screen.
    @ViewBuilder private func nodeMenu(_ n: ScheduleNode) -> some View {
        let bound = n.usesModel && !n.routeId.isEmpty
        let route = routes.first { $0.id == n.routeId }
        let pooled = settings.pool.contains { $0.id == n.routeId && $0.expiresAt > now }
        let linked = graph?.edges.contains { $0.from == n.id || $0.to == n.id } ?? false
        let target = route.map { $0.source + " / " + $0.model } ?? n.routeId
        let status = connectivityLabel(route?.connectivity ?? "") + " · " + target
        Text(n.title)
        if bound { Text(status) }
        Divider()
        Button("编辑节点", systemImage: "pencil") { nodeId = n.id; workflowSettings = false; showInspector = true }
        Button("检测连通性", systemImage: "arrow.clockwise") {
            request(params: .object(["action": .string("check"), "route_id": .string(n.routeId), "workflow_id": .string(workflowId), "luna_supervision": .bool(graph?.lunaSupervision ?? probeSupervision)]))
        }.disabled(!bound || busy || !settings.enabled || dirty)
        if bound && (!settings.enabled || dirty) {
            Text(settings.enabled ? "保存修改后才能检测" : "调度总开关已关闭，需在「来源与模型池」开启并保存后才能检测")
        }
        Button(pooled ? "续期模型池" : "加入模型池", systemImage: "plus") { pool(n.routeId) }.disabled(!bound)
        Divider()
        Button("从此节点发起任务线", systemImage: "arrow.right") { nodeId = n.id; connectionMode = .task; beginConnection(n.id, mode: .task) }
        Button("从此节点发起回报线", systemImage: "arrow.turn.up.left") { nodeId = n.id; connectionMode = .report; beginConnection(n.id, mode: .report) }
        Button("断开连线", systemImage: "scissors") {
            mutate { g in g.edges.removeAll { $0.from == n.id || $0.to == n.id } }
        }.disabled(!linked)
        Divider()
        Button("删除节点", systemImage: "trash", role: .destructive) {
            mutate { g in g.nodes.removeAll { $0.id == n.id }; g.edges.removeAll { $0.from == n.id || $0.to == n.id } }
            if nodeId == n.id { nodeId = nil; showInspector = false }
        }
    }
    private func nodeCard(_ n:ScheduleNode)->some View {
        VStack(alignment:.leading,spacing:6) {
            Text(n.title).font(.headline).lineLimit(1)
            Text(n.nodeType == "codex" ? "真实调用方 · 规划与验收" : (routes.first {$0.id==n.routeId}?.model ?? n.nodeType)).font(.caption).lineLimit(1)
            Text(n.tags.joined(separator:" · ")).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
        }.padding(12).frame(width:220,height:112,alignment:.leading)
        .background(nodeId==n.id ? Color.accentColor.opacity(0.14):Color(nsColor:.controlBackgroundColor),in:RoundedRectangle(cornerRadius:10))
        .overlay(RoundedRectangle(cornerRadius:10).stroke(dropTarget==n.id ? (n.usesModel ? Color.green:Color.red):(nodeId==n.id ? Color.accentColor:Color.secondary),lineWidth:nodeId==n.id || dropTarget==n.id ? 3:1.5))
        .overlay(alignment: .leading) {
            ZStack {
                Circle().fill(Color.accentColor).frame(width: 14, height: 14).offset(y: -12)
                    .opacity(["input", "codex"].contains(n.nodeType) || connectionMode == .report ? 0.2 : 1)
                    .allowsHitTesting(connectionMode != .report && !["input", "codex"].contains(n.nodeType))
                    .onTapGesture { if !["input", "codex"].contains(n.nodeType) { connect(to: n.id, kind: .task) } }
                Circle().fill(from == n.id && fromMode.kind == .report ? Color.green : Color.orange).frame(width: 14, height: 14).offset(y: 22)
                    .opacity(connectionMode == .report ? 1 : 0.3).allowsHitTesting(connectionMode == .report)
                    .onTapGesture { beginConnection(n.id, mode: .report) }
            }.frame(width: 14, height: 112).offset(x: -7)
        }
        .overlay(alignment: .trailing) {
            ZStack {
                Circle().fill(from == n.id && fromMode.kind == .task ? Color.green : Color.accentColor).frame(width: 14, height: 14).offset(y: -12)
                    .opacity(connectionMode == .report ? 0.3 : 1).allowsHitTesting(connectionMode != .report)
                    .onTapGesture { beginConnection(n.id, mode: connectionMode == .bidirectional ? .bidirectional : .task) }
                Circle().fill(Color.orange).frame(width: 14, height: 14).offset(y: 22).opacity(n.nodeType == "input" ? 0.2 : 1)
                    .onTapGesture { if n.nodeType != "input" { connect(to: n.id, kind: .report) } }
            }.frame(width: 14, height: 112).offset(x: 7)
        }
        .overlay(alignment:.bottomTrailing) {if nodeId==n.id {Text("✓ 已选中").font(.caption2).foregroundStyle(Color.accentColor).padding(8)}}
        .scaleEffect(draggedNode==n.id ? 1.035:1.0)
        .shadow(color:Color.accentColor.opacity(draggedNode==n.id ? 0.28:0),radius:12,y:5)
        .animation(.easeOut(duration:0.14),value:draggedNode)
        .onTapGesture {withAnimation(.easeOut(duration:0.12)){nodeId=n.id}}
        .gesture(DragGesture(minimumDistance:4).onChanged {v in
            if origin==nil {origin=CGPoint(x:n.x,y:n.y);nodeId=n.id;withAnimation(.easeOut(duration:0.12)){draggedNode=n.id}};guard let start=origin else {return}
            mutate {g in if let i=g.nodes.firstIndex(where:{$0.id==n.id}) {g.nodes[i].x=min(10000,max(-10000,start.x+v.translation.width/zoom));g.nodes[i].y=min(10000,max(-10000,start.y+v.translation.height/zoom))}}
        }.onEnded {_ in origin=nil;withAnimation(.easeOut(duration:0.16)){draggedNode=nil}})
        .dropDestination(for:String.self) {values,_ in guard let id=values.first,routes.contains(where:{$0.id==id}) else {return false};guard n.usesModel else {error="本地节点不绑定模型；请拖到空白处创建模型节点，或先套用模型职责模板";return true};mutate {g in if let i=g.nodes.firstIndex(where:{$0.id==n.id}) {g.nodes[i].routeId=id}};pool(id);return true} isTargeted: {active in if active {dropTarget=n.id} else if dropTarget==n.id {dropTarget=nil}}
        .contextMenu { nodeMenu(n) }
        .help("任务端口：强调色右出左入；回报端口：橙色左出右入。拖动主体移动，右键打开节点菜单。")
    }
    private func fitCanvas() {
        guard let nodes=graph?.nodes,!nodes.isEmpty else {pan=CGPoint(x:24,y:24);zoom=1;return}
        let minX=nodes.map(\.x).min() ?? 0,minY=nodes.map(\.y).min() ?? 0
        let width=(nodes.map {$0.x+220}.max() ?? 220)-minX,height=(nodes.map {$0.y+112}.max() ?? 112)-minY
        zoom=min(1.8,max(0.02,min(max(1,viewport.width-48)/width,max(1,viewport.height-48)/height)))
        pan=CGPoint(x:(viewport.width-width*zoom)/2-minX*zoom,y:(viewport.height-height*zoom)/2-minY*zoom)
    }
    private func zoomCanvas(_ delta:Double) {
        let anchor=CGPoint(x:viewport.width/2,y:viewport.height/2),world=CGPoint(x:(viewport.width/2-pan.x)/zoom,y:(viewport.height/2-pan.y)/zoom)
        zoom=min(1.8,max(0.02,zoom+delta));pan=CGPoint(x:anchor.x-world.x*zoom,y:anchor.y-world.y*zoom)
    }

    /// Node editor as form sections: identity, task, fallbacks, template and
    /// advanced parameters. ↑/↓ share one ControlGroup (they used to sit on
    /// two separate lines of their own), destructive actions sit last.
    @ViewBuilder private func editor(_ i: Int, _ n: ScheduleNode) -> some View {
        Section("节点") {
            TextField("名称", text: nodeBinding(i, \.title))
            TextField("分类标签", text: Binding(get: { n.tags.joined(separator: ", ") }, set: { v in mutate { $0.nodes[i].tags = v.split(whereSeparator: { $0 == "," || $0 == "，" }).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty } } }), prompt: Text("逗号分隔"))
            LabeledContent("绑定模型") {
                Text(routes.first { $0.id == n.routeId }.map { "\($0.source) / \($0.model)" } ?? (n.routeId.isEmpty ? "未绑定" : n.routeId))
                    .foregroundStyle(.secondary).lineLimit(2).truncationMode(.middle).textSelection(.enabled)
            }
            LabeledContent("执行优先级") {
                ControlGroup {
                    Button { moveNode(i, -1) } label: { Image(systemName: "chevron.up") }.help("提前")
                    Button { moveNode(i, 1) } label: { Image(systemName: "chevron.down") }.help("延后")
                }.fixedSize()
            }
        }
        Section {
            TextField("职责", text: nodeBinding(i, \.role), axis: .vertical).lineLimit(2...5)
            TextField("提示词", text: Binding(get: { n.task["prompt"].text }, set: { v in mutate { $0.nodes[i].task["prompt"] = .string(v) } }), axis: .vertical).lineLimit(2...6)
        } header: { Text("任务") } footer: {
            Text("职责说明节点负责什么、交付什么；提示词给出具体任务与验收条件，运行时会附上本次输入和上游报告。").foregroundStyle(.secondary)
        }
        if n.usesModel {
            Section("备用 / 补位路线") {
                if n.fallbackRoutes.isEmpty { Text("未设置").foregroundStyle(.secondary) }
                else { Text(n.fallbackRoutes.map { id in routes.first { $0.id == id }?.model ?? id }.joined(separator: " → ")).lineLimit(3) }
                HStack {
                    Menu("从模型池添加") { ForEach(settings.pool.filter { $0.expiresAt > now && $0.id != n.routeId && !n.fallbackRoutes.contains($0.id) }) { p in Button(routes.first { $0.id == p.id }?.model ?? p.id) { mutate { g in if g.nodes[i].fallbackRoutes.count < 8 { g.nodes[i].fallbackRoutes.append(p.id) } } } } }.fixedSize()
                    Spacer()
                    Button("清空") { mutate { $0.nodes[i].fallbackRoutes = [] } }.disabled(n.fallbackRoutes.isEmpty)
                }
            }
        }
        Section {
            DisclosureGroup("套用职责模板") {
                VStack(alignment:.leading,spacing:8) {
                    ForEach(templates) {t in
                        Button {applyTemplate(t,to:i)} label: {
                            VStack(alignment:.leading,spacing:4) {
                                Text(t.title).font(.body.weight(.semibold))
                                Text(t.role).font(.caption).foregroundStyle(.secondary)
                            }.frame(maxWidth:.infinity,alignment:.leading)
                                .fixedSize(horizontal:false,vertical:true).padding(6)
                        }.buttonStyle(.bordered)
                    }
                }.frame(maxWidth:.infinity,alignment:.leading).multilineTextAlignment(.leading)
            }
            DisclosureGroup("高级调用参数") {
                VStack(alignment:.leading,spacing:8) {
                    Text("输出与等待参数留空可继承配置档；模型原生容量和共享预算仍有效。").font(.caption).foregroundStyle(.secondary)
                    ForEach(["kind", "max_output_tokens", "token_budget", "deadline_s", "hard_deadline_s", "ttft_s", "idle_s", "output_mode", "report_max_chars"], id: \.self) { key in
                        TextField(parameterName(key), text: Binding(get: { if case .number(let v) = n.task[key] { return String(Int(v)) }; return n.task[key].text }, set: { v in mutate { g in if v.isEmpty && !["kind","prompt"].contains(key) {var fields=g.nodes[i].task.fields;fields.removeValue(forKey:key);g.nodes[i].task = .object(fields)} else {g.nodes[i].task[key] = ["kind", "prompt", "output_mode"].contains(key) ? .string(v) : (Double(v).map(SchedulerJSON.number) ?? .null)} } }))
                    }
                    Text("高级节点参数 JSON").font(.caption)
                    TextEditor(text: $taskText).font(.system(.body, design: .monospaced)).frame(height: 140).border(.quaternary)
                    Button("应用高级参数") { do { let value = try JSONDecoder().decode(SchedulerJSON.self, from: Data(taskText.utf8)); guard case .object = value else { return }; mutate { $0.nodes[i].task = value } } catch { self.error = error.localizedDescription } }
                }.frame(maxWidth:.infinity,alignment:.leading).multilineTextAlignment(.leading)
            }
        }
        Section {
            Button("移除节点", role: .destructive) { mutate { g in g.nodes.remove(at: i); g.edges.removeAll { $0.from == n.id || $0.to == n.id } }; nodeId = nil }
        }
    }
    private func parameterName(_ key:String)->String {switch key {case "kind":return "任务类型";case "max_output_tokens":return "最大输出 Tokens";case "token_budget":return "节点 Token 预算";case "deadline_s":return "首次响应期待（秒）";case "hard_deadline_s":return "硬截止（秒）";case "ttft_s":return "首次活动等待（秒）";case "idle_s":return "停滞窗口（秒）";case "output_mode":return "输出模式 fixed / adaptive / native";case "report_max_chars":return "报告字符上限";default:return key}}
    private func graphBinding<T>(_ path: WritableKeyPath<ScheduleGraph, T>) -> Binding<T> {
        Binding(get: { settings.workflows[selected][keyPath: path] }, set: { v in mutate { $0[keyPath: path] = v } })
    }
    private func nodeBinding<T>(_ index: Int, _ path: WritableKeyPath<ScheduleNode, T>) -> Binding<T> {
        Binding(get: { settings.workflows[selected].nodes[index][keyPath: path] }, set: { v in mutate { $0.nodes[index][keyPath: path] = v } })
    }
    private func mutate(_ body: (inout ScheduleGraph) -> Void) { guard settings.workflows.indices.contains(selected) else { return }; body(&settings.workflows[selected]); dirty = true }
    private func pool(_ id: String) { let expiry = now + UInt64(minutes) * 60000; if let i = settings.pool.firstIndex(where: { $0.id == id }) { settings.pool[i].expiresAt = expiry } else if settings.pool.count < 128 { settings.pool.append(SchedulePool(routeId: id, expiresAt: expiry)) }; dirty = true }
    private func movePool(_ i: Int, _ delta: Int) { let j = i + delta; if settings.pool.indices.contains(j) { settings.pool.swapAt(i, j); dirty = true } }
    private func moveNode(_ i: Int, _ delta: Int) { let j = i + delta; mutate { if $0.nodes.indices.contains(j) { $0.nodes.swapAt(i, j) } } }
    private func addCodex() {
        addTemplate(ScheduleTemplate(id: "codex", title: "母节点 · Codex 接口", nodeType: "codex",
            role: "由真实 Codex 调用方查看完整拓扑，规划切片、分配任务、确认修正并验收结果。",
            tags: ["母节点", "规划", "验收"], task: .object(["kind": .string("extract"), "prompt": .string("等待真实 Codex 的规划与验收回执；不自动启动模型。")])) )
    }
    private func addTemplate(_ t: ScheduleTemplate) {
        if graph == nil { var g = ScheduleGraph(); g.lunaSupervision = probeSupervision; settings.workflows.append(g); selected = settings.workflows.count - 1 }
        guard let g = graph, g.nodes.count < 256 else { return }
        let route = settings.pool.first(where: { $0.expiresAt > now })?.routeId ?? ""
        let id = UUID().uuidString
        mutate { $0.nodes.append(ScheduleNode(id: id, title: t.title, routeId: ["model", "operator", "reviewer"].contains(t.nodeType) ? route : "", nodeType: t.nodeType, role: t.role, tags: t.tags, x: Double(g.nodes.count % 3) * 250 + 40, y: Double(g.nodes.count / 3) * 160 + 40,
            task: .object(["kind": .string("extract"), "prompt": .string("分析输入材料。"), "max_output_tokens": .number(8192), "token_budget": .number(48000), "deadline_s": .number(300), "hard_deadline_s": .number(900)]))) }
        if let i = graph?.nodes.firstIndex(where: { $0.id == id }) { applyTemplate(t, to: i) }; nodeId = id
    }
    private func applyTemplate(_ t: ScheduleTemplate, to i: Int) {
        mutate { g in
            g.nodes[i].title = t.title; g.nodes[i].nodeType = t.nodeType; g.nodes[i].role = t.role; g.nodes[i].tags = t.tags
            if !["model", "operator", "reviewer"].contains(t.nodeType) { g.nodes[i].routeId = ""; g.nodes[i].fallbackRoutes = [] }
            if case .object(let values) = t.task { for (key, value) in values { g.nodes[i].task[key] = value } }
        }
        taskText = graph?.nodes[i].task.pretty ?? ""
    }
    private func beginConnection(_ id: String, mode: ScheduleConnectionMode) {
        from = id; fromMode = mode; error = nil
    }
    private func connect(to id: String, kind: ScheduleEdgeKind) {
        guard let source = from, source != id, fromMode.kind == kind, let original = graph else { return }
        let mode = fromMode
        var candidate = original
        candidate.edges.append(ScheduleEdge(from: source, to: id, kind: kind))
        if mode == .bidirectional { candidate.edges.append(ScheduleEdge(from: id, to: source, kind: .report)) }
        Task { do {
            let signature = try SchedulerJSON.encode(original).pretty
            _ = try await Core.scheduling(SchedulerJSON.object(["method": .string("validate_graph"), "params": try SchedulerJSON.encode(candidate)]).pretty)
            guard let current = graph, current.id == original.id,
                  try SchedulerJSON.encode(current).pretty == signature,
                  from == source, fromMode == mode else {
                error = "连线期间工作流已变化，请重新连接。"; return
            }
            mutate { $0 = candidate }; from = nil
        } catch { self.error = error.localizedDescription } }
    }
    private func save() { do { request(params: .object(["action": .string("save"), "expected_revision": .number(Double(settings.revision)), "value": try SchedulerJSON.encode(settings)]), replace: true) } catch { self.error = error.localizedDescription } }
    private func run() { showRun=true;guard let graph else { return }; do {
        if let reason = operatorAccessError { error = reason; return }
        let definition = try SchedulerJSON.encode(graph); let grant = executionGrant
        let signature = definition.pretty + input + workflowId + (grant?.pretty ?? "")
        if submitKey.isEmpty || signature != submitSignature || finished { submitKey = UUID().uuidString; submitSignature = signature; finished = false }
        var params: [String: SchedulerJSON] = ["definition": definition, "input_text": .string(input), "workflow_id": .string(workflowId), "request_id": .string(submitKey), "acknowledge_unknown_risk": .bool(acknowledge)]
        if let grant { params["execution"] = grant }
        request("router_workflow_submit", params: .object(params))
    } catch { self.error = error.localizedDescription } }
    private func poll(_ id: String) { guard !id.isEmpty else { return }; request("router_workflow_status", params: .object(["run_id": .string(id)])) }
    private func request(_ method: String = "router_scheduling", params: SchedulerJSON = .object([:]), replace: Bool = false) {
        guard !busy else { return }; busy = true; error = nil
        Task { do {
            let value = try await Core.scheduling(SchedulerJSON.object(["method": .string(method), "params": params]).pretty)
            if case .object = value["settings"] {
                if !dirty || replace { settings = try value["settings"].decode(ScheduleSettings.self); selected = min(selected, max(0, settings.workflows.count - 1)); nodeId = nil; profileText = settings.callProfile.pretty; dirty = false }
                routes = try value["routes"].decode([ScheduleRoute].self)
                supervisorWorkflows=value["supervisors"].values.map {$0["workflow_id"].text}
                sources = Array(Set(value["sources"].values.map { $0["id"].text } + routes.map(\.source))).filter { !$0.isEmpty }.sorted()
                runs = value["runs"].values
                templates = try value["templates"].decode([ScheduleTemplate].self)
                callPresets = value["call_presets"].values
                effectiveProfile = value["effective_profile"].pretty
                if workflowId.isEmpty { workflowId = value["supervisors"].values.first?["workflow_id"].text ?? "default" }
                if runId.isEmpty, let active = runs.first(where: { $0["workflow_id"].text == workflowId && ["running", "cancelling"].contains($0["state"].text) }) {
                    runId = active["id"].text
                    showRun = true
                    detailValue = active
                    Task { @MainActor in
                        await Task.yield()
                        poll(runId)
                    }
                }
                if case .object = value["check"] { notice = value["check"].pretty }
            } else if value.fields.keys.contains("results") { report=value.pretty }
            else { if !value["run_id"].text.isEmpty { runId = value["run_id"].text }; detail = value.pretty; detailValue=value; finished = ["succeeded", "failed", "cancelled", "interrupted"].contains(value["state"].text) }
        } catch { self.error = error.localizedDescription }; busy = false }
    }
}
