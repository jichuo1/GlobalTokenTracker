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
}
private struct ScheduleEdge: Codable { var from: String; var to: String }
private struct ScheduleGraph: Codable, Identifiable {
    var id = UUID().uuidString
    var name = "定向工作流"
    var nodes: [ScheduleNode] = []
    var edges: [ScheduleEdge] = []
    var tokenBudget: UInt64 = 200000
    var maxParallel = 2
    var lunaSupervision = true
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
    @ViewState private var settings = ScheduleSettings()
    @ViewState private var routes: [ScheduleRoute] = []
    @ViewState private var sources: [String] = []
    @ViewState private var runs: [SchedulerJSON] = []
    @ViewState private var selected = 0
    @ViewState private var nodeId: String?
    @ViewState private var from: String?
    @ViewState private var query = ""
    @ViewState private var minutes = 120
    @ViewState private var busy = false
    @ViewState private var dirty = false
    @ViewState private var error: String?
    @ViewState private var notice = ""
    @ViewState private var workflowId = ""
    @ViewState private var input = ""
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
                guard ["running","cancelling"].contains(detailValue["state"].text) else {break}
                poll(id)
            }
        }
        .onChange(of: nodeId) { _, _ in taskText = nodeIndex.flatMap { graph?.nodes[$0].task.pretty } ?? "" }
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
    private var running: Bool { ["running","cancelling"].contains(detailValue["state"].text) }
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
                    Text("拖动节点主体移动 · 从右端口连接下游").font(.caption).foregroundStyle(.secondary)
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
        if ["running","cancelling"].contains(detailValue["state"].text) {return "已有运行进行中，可查看结果或停止。"}
        guard let g=graph,!g.nodes.isEmpty else {return "先从模型库拖入模型，或通过添加节点选择职责模板。"}
        if dirty {return "请先保存修改，再运行工作流。"}
        if !settings.enabled {return "调度总开关已关闭，请在来源与模型池中检查。"}
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
        }.labelsHidden().frame(width: 220).onChange(of:selected) { _,_ in nodeId=nil }
    }
    private var addMenu: some View {
        Menu {
            ForEach(templates) {t in Button(t.title) {addTemplate(t)}.help(t.role)}
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
            Divider()
            Section("操作说明") {
                Text("空白处拖动平移；拖节点主体移动。")
                Text("点击右端口后，再点击下游左端口连线。")
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
                Button("按顺序串联") {mutate {g in g.edges=zip(g.nodes,g.nodes.dropFirst()).map {ScheduleEdge(from:$0.0.id,to:$0.1.id)}}}
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
                        GridRow {
                            Text("付费风险")
                            Toggle("新建未知结果的运行前，接受重复付费风险",isOn:$acknowledge)
                        }
                    }
                    if !runs.isEmpty {
                        Text("最近运行").font(.subheadline.weight(.semibold))
                        LazyVGrid(columns:[GridItem(.adaptive(minimum:200),spacing:8)],alignment:.leading,spacing:8) {
                            ForEach(runs.indices,id:\.self) {i in Button("\(runs[i]["name"].text) · \(runs[i]["state"].text)") {runId=runs[i]["id"].text;poll(runId)}}
                        }
                    }
                    let jobs = detailValue["nodes"].fields.keys.sorted().filter {!detailValue["nodes"][$0]["job_id"].text.isEmpty}
                    if !jobs.isEmpty {
                        Text("节点报告").font(.subheadline.weight(.semibold))
                        LazyVGrid(columns:[GridItem(.adaptive(minimum:200),spacing:8)],alignment:.leading,spacing:8) {
                            ForEach(jobs,id:\.self) {id in Button("查看节点 \(id) 报告") {request("router_collect",params:.object(["job_id":detailValue["nodes"][id]["job_id"],"max_chars":.number(12000)]))}}
                        }
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
                Canvas {context,size in
                    let grid=Color.secondary.opacity(0.15)
                    for x in stride(from:pan.x.truncatingRemainder(dividingBy:24),to:size.width,by:24) {var p=Path();p.move(to:CGPoint(x:x,y:0));p.addLine(to:CGPoint(x:x,y:size.height));context.stroke(p,with:.color(grid),lineWidth:0.5)}
                    for y in stride(from:pan.y.truncatingRemainder(dividingBy:24),to:size.height,by:24) {var p=Path();p.move(to:CGPoint(x:0,y:y));p.addLine(to:CGPoint(x:size.width,y:y));context.stroke(p,with:.color(grid),lineWidth:0.5)}
                    if let g=graph {let points=Dictionary(uniqueKeysWithValues:g.nodes.map {($0.id,$0)});if let from,let a=points[from],let pointer=hoverPoint {var p=Path();p.move(to:CGPoint(x:pan.x+(a.x+220)*zoom,y:pan.y+(a.y+56)*zoom));p.addLine(to:pointer);context.stroke(p,with:.color(.accentColor),style:StrokeStyle(lineWidth:2,dash:[5,4]))};for e in g.edges {if let a=points[e.from],let b=points[e.to] {
                        var p=Path();p.move(to:CGPoint(x:pan.x+(a.x+220)*zoom,y:pan.y+(a.y+56)*zoom));p.addLine(to:CGPoint(x:pan.x+b.x*zoom,y:pan.y+(b.y+56)*zoom));context.stroke(p,with:.color(.accentColor),lineWidth:2)
                    }}}
                }.allowsHitTesting(false)
                if let g=graph {ForEach(g.nodes) {n in nodeCard(n).scaleEffect(zoom,anchor:.topLeading).offset(x:pan.x+n.x*zoom,y:pan.y+n.y*zoom).allowsHitTesting(!panTool)}}
            }.frame(width:geometry.size.width,height:geometry.size.height).clipped()
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
    private func nodeCard(_ n:ScheduleNode)->some View {
        VStack(alignment:.leading,spacing:6) {
            Text(n.title).font(.headline).lineLimit(1)
            Text(routes.first {$0.id==n.routeId}?.model ?? n.nodeType).font(.caption).lineLimit(1)
            Text(n.tags.joined(separator:" · ")).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
        }.padding(12).frame(width:220,height:112,alignment:.leading)
        .background(nodeId==n.id ? Color.accentColor.opacity(0.14):Color(nsColor:.controlBackgroundColor),in:RoundedRectangle(cornerRadius:10))
        .overlay(RoundedRectangle(cornerRadius:10).stroke(dropTarget==n.id ? (n.nodeType=="model" ? Color.green:Color.red):(nodeId==n.id ? Color.accentColor:Color.secondary),lineWidth:nodeId==n.id || dropTarget==n.id ? 3:1.5))
        .overlay(alignment:.leading) {Circle().fill(Color.accentColor).frame(width:14,height:14).onTapGesture {connect(to:n.id)}}
        .overlay(alignment:.trailing) {Circle().fill(from==n.id ? Color.orange:Color.accentColor).frame(width:14,height:14).onTapGesture {from=n.id}}
        .overlay(alignment:.bottomTrailing) {if nodeId==n.id {Text("✓ 已选中").font(.caption2).foregroundStyle(Color.accentColor).padding(8)}}
        .scaleEffect(draggedNode==n.id ? 1.035:1.0)
        .shadow(color:Color.accentColor.opacity(draggedNode==n.id ? 0.28:0),radius:12,y:5)
        .animation(.easeOut(duration:0.14),value:draggedNode)
        .onTapGesture {withAnimation(.easeOut(duration:0.12)){nodeId=n.id}}
        .gesture(DragGesture(minimumDistance:4).onChanged {v in
            if origin==nil {origin=CGPoint(x:n.x,y:n.y);nodeId=n.id;withAnimation(.easeOut(duration:0.12)){draggedNode=n.id}};guard let start=origin else {return}
            mutate {g in if let i=g.nodes.firstIndex(where:{$0.id==n.id}) {g.nodes[i].x=min(10000,max(-10000,start.x+v.translation.width/zoom));g.nodes[i].y=min(10000,max(-10000,start.y+v.translation.height/zoom))}}
        }.onEnded {_ in origin=nil;withAnimation(.easeOut(duration:0.16)){draggedNode=nil}})
        .dropDestination(for:String.self) {values,_ in guard let id=values.first,routes.contains(where:{$0.id==id}) else {return false};guard n.nodeType=="model" else {error="本地节点不绑定模型；请拖到空白处创建模型节点，或先套用模型职责模板";return true};mutate {g in if let i=g.nodes.firstIndex(where:{$0.id==n.id}) {g.nodes[i].routeId=id}};pool(id);return true} isTargeted: {active in if active {dropTarget=n.id} else if dropTarget==n.id {dropTarget=nil}}
        .help("点击选中；拖动主体移动，从右端口连接下游。")
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
        if n.nodeType == "model" {
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
    private func addTemplate(_ t: ScheduleTemplate) {
        if graph == nil { var g = ScheduleGraph(); g.lunaSupervision = probeSupervision; settings.workflows.append(g); selected = settings.workflows.count - 1 }
        guard let g = graph, g.nodes.count < 256 else { return }
        let route = settings.pool.first(where: { $0.expiresAt > now })?.routeId ?? ""
        let id = UUID().uuidString
        mutate { $0.nodes.append(ScheduleNode(id: id, title: t.title, routeId: t.nodeType == "model" ? route : "", nodeType: t.nodeType, role: t.role, tags: t.tags, x: Double(g.nodes.count % 3) * 250 + 40, y: Double(g.nodes.count / 3) * 160 + 40,
            task: .object(["kind": .string("extract"), "prompt": .string("分析输入材料。"), "max_output_tokens": .number(8192), "token_budget": .number(48000), "deadline_s": .number(300), "hard_deadline_s": .number(900)]))) }
        if let i = graph?.nodes.firstIndex(where: { $0.id == id }) { applyTemplate(t, to: i) }; nodeId = id
    }
    private func applyTemplate(_ t: ScheduleTemplate, to i: Int) {
        mutate { g in
            g.nodes[i].title = t.title; g.nodes[i].nodeType = t.nodeType; g.nodes[i].role = t.role; g.nodes[i].tags = t.tags
            if t.nodeType != "model" { g.nodes[i].routeId = ""; g.nodes[i].fallbackRoutes = [] }
            if case .object(let values) = t.task { for (key, value) in values { g.nodes[i].task[key] = value } }
        }
        taskText = graph?.nodes[i].task.pretty ?? ""
    }
    private func connect(to id: String) {
        guard let from, from != id, var candidate = graph else { return }
        candidate.edges.append(ScheduleEdge(from: from, to: id))
        Task { do { _ = try await Core.scheduling(SchedulerJSON.object(["method": .string("validate_graph"), "params": try SchedulerJSON.encode(candidate)]).pretty); mutate { $0 = candidate }; self.from = nil } catch { self.error = error.localizedDescription } }
    }
    private func save() { do { request(params: .object(["action": .string("save"), "expected_revision": .number(Double(settings.revision)), "value": try SchedulerJSON.encode(settings)]), replace: true) } catch { self.error = error.localizedDescription } }
    private func run() { showRun=true;guard let graph else { return }; do {
        let definition = try SchedulerJSON.encode(graph); let signature = definition.pretty + input + workflowId
        if submitKey.isEmpty || signature != submitSignature || finished { submitKey = UUID().uuidString; submitSignature = signature; finished = false }
        request("router_workflow_submit", params: .object(["definition": definition, "input_text": .string(input), "workflow_id": .string(workflowId), "request_id": .string(submitKey), "acknowledge_unknown_risk": .bool(acknowledge)]))
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
