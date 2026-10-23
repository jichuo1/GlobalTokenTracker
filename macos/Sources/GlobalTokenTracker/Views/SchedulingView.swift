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
    @State private var settings = ScheduleSettings()
    @State private var routes: [ScheduleRoute] = []
    @State private var sources: [String] = []
    @State private var runs: [SchedulerJSON] = []
    @State private var selected = 0
    @State private var nodeId: String?
    @State private var from: String?
    @State private var query = ""
    @State private var minutes = 120
    @State private var busy = false
    @State private var dirty = false
    @State private var error: String?
    @State private var notice = ""
    @State private var workflowId = ""
    @State private var input = ""
    @State private var runId = ""
    @State private var detail = ""
    @State private var zoom = 1.0
    @State private var origin: CGPoint?
    @State private var pan = CGPoint(x: 24, y: 24)
    @State private var panOrigin: CGPoint?
    @State private var viewport = CGSize(width: 800, height: 500)
    @State private var panTool = false
    @State private var showInspector = false
    @State private var showRun = false
    @State private var showLibrary = true
    @State private var templates: [ScheduleTemplate] = []
    @State private var probeSupervision = true
    @State private var taskText = ""
    @State private var profileText = ""
    @State private var callPresets: [SchedulerJSON] = []
    @State private var effectiveProfile = ""
    @State private var tab = "workflow"
    @State private var submitKey = ""
    @State private var submitSignature = ""
    @State private var finished = false
    @State private var acknowledge = false
    @State private var detailValue: SchedulerJSON = .null
    @State private var report = ""

    private var graph: ScheduleGraph? { settings.workflows.indices.contains(selected) ? settings.workflows[selected] : nil }
    private var nodeIndex: Int? { graph?.nodes.firstIndex { $0.id == nodeId } }
    private var filtered: [ScheduleRoute] { routes.filter { query.isEmpty || "\($0.source) \($0.model)".localizedCaseInsensitiveContains(query) } }
    private var now: UInt64 { UInt64(Date().timeIntervalSince1970 * 1000) }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("模型调度").font(.title2.bold())
                Spacer()
                if busy { ProgressView().controlSize(.small) }
                Text(dirty ? "未保存" : "已同步").font(.caption).foregroundStyle(.secondary)
                Button("保存") { save() }.disabled(busy)
                Menu("服务") {
                    Button("启动服务") { request("router_start") }
                    Button("刷新状态") { request() }
                    Button("重新扫描来源") { request(params: .object(["action": .string("refresh")])) }
                    Button("放弃修改并重载") { dirty = false; request() }
                }.disabled(busy)
            }
            Picker("调度视图", selection: $tab) {
                Text("工作流").tag("workflow"); Text("来源与模型池").tag("sources"); Text("调用配置").tag("profiles")
            }.pickerStyle(.segmented).frame(maxWidth: 600)
            if let error { Text(error).foregroundStyle(.red).font(.caption).lineLimit(3).textSelection(.enabled) }
            if !notice.isEmpty { Text(notice).font(.caption).lineLimit(2).textSelection(.enabled) }
            if tab == "workflow" { workspace }
            else { ScrollView { VStack(alignment: .leading, spacing: 12) {
                if tab == "sources" { sourcePanel } else { profilePanel }
            }.frame(maxWidth: .infinity, alignment: .leading) } }
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

    @ViewBuilder private var sourcePanel: some View {
        Toggle("允许模型调度", isOn: Binding(get: {settings.enabled}, set: {settings.enabled=$0;dirty=true}))
        Toggle("定向模式 · 仅使用限时池", isOn: Binding(get: {settings.mode=="directed"}, set: {settings.mode=$0 ? "directed":"auto";dirty=true}))
        Toggle("手动检测启用 Luna", isOn: $probeSupervision)
        Text("目录与认证提示不代表生成可用。检测占用共享预算，可按当前工作流选择监工。").font(.caption).foregroundStyle(.secondary)
                ForEach(sources, id: \.self) { source in
                    Toggle("\(source) · \(routes.filter { $0.source == source }.count) 模型", isOn: Binding(
                        get: { settings.sources[source] ?? true }, set: { settings.sources[source] = $0; dirty = true }))
                }
                HStack { TextField("搜索模型 / 来源", text: $query); Stepper("有效期 \(minutes) 分钟", value: $minutes, in: 1...10080) }
                Text("目录可见不代表生成可用。检测会产生少量用量，监工可选，并遵守共享探针预算。")
                    .font(.caption).foregroundStyle(.secondary)
                ScrollView { LazyVStack(alignment: .leading) {
                    ForEach(filtered) { route in
                        HStack {
                            Text("\(route.source) / \(route.model)").draggable(route.id)
                            Spacer()
                            Text(route.connectivity).font(.caption)
                            Button("加入 / 续期") { pool(route.id) }
                            Button("生成检测") { request(params: .object(["action": .string("check"), "route_id": .string(route.id), "workflow_id": .string(workflowId), "luna_supervision": .bool(graph?.lunaSupervision ?? probeSupervision)])) }.disabled(busy)
                        }.padding(5)
                    }
                }}.frame(height: 280)
                Divider()
                Text("限时模型池").font(.headline)
                ForEach(Array(settings.pool.enumerated()), id: \.element.id) { index, entry in
                    HStack {
                        Text("\(index + 1) · \(routes.first { $0.id == entry.id }?.model ?? entry.id)")
                        Text(entry.expiresAt > now ? "剩余 \((entry.expiresAt - now) / 60000) 分钟" : "已到期").font(.caption)
                        Spacer()
                        Button("↑") { movePool(index, -1) }
                        Button("↓") { movePool(index, 1) }
                        Button("移出") { settings.pool.remove(at: index); dirty = true }
                    }
                }
                Divider()

    }

    @ViewBuilder private var profilePanel: some View {
                    Text("节点显式参数优先于预设默认值；配置档上限与模型原生上限共同生效。")
                    LazyVGrid(columns: Array(repeating: GridItem(.flexible()), count: 3)) {
                        ForEach(callPresets.indices, id: \.self) { i in Button(callPresets[i]["title"].text) { settings.callProfile = callPresets[i]["value"]; profileText = settings.callProfile.pretty; dirty = true } }
                    }
                    TextEditor(text: $profileText).frame(height: 220).border(.quaternary)
                    Button("应用配置到待保存设置") { do { let value = try JSONDecoder().decode(SchedulerJSON.self, from: Data(profileText.utf8)); guard case .object = value else { return }; settings.callProfile = value; dirty = true } catch { self.error = error.localizedDescription } }
                    Text(effectiveProfile).font(.system(.caption, design: .monospaced)).textSelection(.enabled)

    }

    private var workspace: some View {
        VStack(alignment: .leading, spacing: 8) {
            ViewThatFits(in: .horizontal) {
                HStack { workflowPicker; supervisorToggle; Spacer(); executionButtons }
                VStack(alignment: .leading) { HStack { workflowPicker; supervisorToggle }; executionButtons }
            }
            HStack {
                Menu("添加节点") { ForEach(templates) { t in Button(t.title) { addTemplate(t) } } }
                Toggle("平移", isOn: $panTool).toggleStyle(.button)
                Button("适应") { fitCanvas() }
                Button("−") { zoomCanvas(-0.1) }; Text("\(Int(zoom*100))%").font(.caption).monospacedDigit(); Button("+") { zoomCanvas(0.1) }
                Spacer()
                Toggle("模型库", isOn: $showLibrary).toggleStyle(.button)
                Toggle("参数", isOn: $showInspector).toggleStyle(.button)
                Toggle("输入与结果", isOn: $showRun).toggleStyle(.button)
            }
            Text("空白处拖动平移 · 拖动节点移动 · 点击右端口后点击目标左端口连线").font(.caption).foregroundStyle(.secondary)
            HSplitView {
                if showLibrary { modelLibrary }
                graphCanvas.frame(minWidth: 200, minHeight: 180)
                if showInspector || nodeIndex != nil { inspectorPanel }
            }.frame(maxWidth: .infinity, maxHeight: .infinity)
            if showRun { runPanel.frame(height: 200) }
        }
    }
    private var workflowPicker: some View {
        Picker("工作流", selection: $selected) {
            ForEach(Array(settings.workflows.enumerated()), id: \.element.id) { i,g in Text(g.name).tag(i) }
        }.labelsHidden().frame(width: 230).onChange(of:selected) { _,_ in nodeId=nil }
    }
    private var supervisorToggle: some View {
        Toggle("Luna", isOn: Binding(get: {graph?.lunaSupervision ?? probeSupervision}, set: {v in probeSupervision=v;mutate {$0.lunaSupervision=v}})).toggleStyle(.switch).fixedSize()
    }
    private var executionButtons: some View {
        HStack {
            Button("预检") { guard let graph else {return};do {request("router_workflow_plan",params:.object(["definition":try SchedulerJSON.encode(graph),"workflow_id":.string(workflowId)]))} catch {self.error=error.localizedDescription} }
            Button("运行") {run()}.disabled(dirty || busy || graph==nil)
            Button("停止") {request("router_workflow_cancel",params:.object(["run_id":.string(runId)]))}.disabled(busy || runId.isEmpty)
        }
    }
    private var modelLibrary: some View {
        VStack(alignment:.leading,spacing:8) {
            Text("模型库").font(.headline)
            TextField("搜索模型",text:$query)
            ScrollView {LazyVStack(alignment:.leading) {ForEach(filtered) {r in Text("\(r.source) / \(r.model)").font(.caption).lineLimit(2).padding(6).frame(maxWidth:.infinity,alignment:.leading).draggable(r.id)}}}
        }.padding(8).frame(minWidth:160,idealWidth:190,maxWidth:240)
    }
    private var inspectorPanel: some View {
        ScrollView {
            VStack(alignment:.leading,spacing:10) {
                HStack {Text("参数").font(.headline);Spacer();Button("关闭") {showInspector=false;nodeId=nil}}
                HStack {Button("新建") {settings.workflows.append(ScheduleGraph());selected=settings.workflows.count-1;nodeId=nil;dirty=true}; Button("移除工作流") {if graph != nil {settings.workflows.remove(at:selected);selected=0;nodeId=nil;dirty=true}}}
                if graph != nil {
                    TextField("工作流名称",text:graphBinding(\.name))
                    TextField("总 Token 预算",value:graphBinding(\.tokenBudget),format:.number)
                    Stepper("并发 \(graph?.maxParallel ?? 2)",value:graphBinding(\.maxParallel),in:1...32)
                    Button("按顺序串联") {mutate {g in g.edges=zip(g.nodes,g.nodes.dropFirst()).map {ScheduleEdge(from:$0.0.id,to:$0.1.id)}}}
                    Button("清空连线") {mutate {$0.edges=[]}}
                }
                Divider()
                if let i=nodeIndex,let n=graph?.nodes[i] {editor(i,n)}
            }.padding(10).frame(maxWidth:.infinity,alignment:.leading)
        }.frame(minWidth:270,idealWidth:300,maxWidth:360)
    }
    private var runPanel: some View {
        ScrollView {
            VStack(alignment:.leading,spacing:8) {
                HStack(alignment:.top) {TextField("工作流 ID",text:$workflowId).frame(width:200);TextEditor(text:$input).frame(minHeight:70).border(.quaternary)}
                Toggle("明确接受未知结果的重复付费风险",isOn:$acknowledge)
                ForEach(runs.indices,id:\.self) {i in Button("\(runs[i]["name"].text) · \(runs[i]["state"].text)") {runId=runs[i]["id"].text;poll(runId)}}
                ForEach(detailValue["nodes"].fields.keys.sorted(),id:\.self) {id in
                    if !detailValue["nodes"][id]["job_id"].text.isEmpty {Button("查看节点 \(id) 报告") {request("router_collect",params:.object(["job_id":detailValue["nodes"][id]["job_id"],"max_chars":.number(12000)]))}}
                }
                Text(report.isEmpty ? detail:report).font(.system(.caption,design:.monospaced)).textSelection(.enabled)
            }.frame(maxWidth:.infinity,alignment:.leading)
        }
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
                    if let g=graph {let points=Dictionary(uniqueKeysWithValues:g.nodes.map {($0.id,$0)});for e in g.edges {if let a=points[e.from],let b=points[e.to] {
                        var p=Path();p.move(to:CGPoint(x:pan.x+(a.x+220)*zoom,y:pan.y+(a.y+56)*zoom));p.addLine(to:CGPoint(x:pan.x+b.x*zoom,y:pan.y+(b.y+56)*zoom));context.stroke(p,with:.color(.accentColor),lineWidth:2)
                    }}}
                }.allowsHitTesting(false)
                if let g=graph {ForEach(g.nodes) {n in nodeCard(n).scaleEffect(zoom,anchor:.topLeading).offset(x:pan.x+n.x*zoom,y:pan.y+n.y*zoom).allowsHitTesting(!panTool)}}
            }.frame(width:geometry.size.width,height:geometry.size.height).clipped()
            .onAppear {viewport=geometry.size}.onChange(of:geometry.size) {_,v in viewport=v}
            .dropDestination(for:String.self) {values,point in
                guard let id=values.first,let route=routes.first(where:{$0.id==id}) else {return false}
                if graph==nil {settings.workflows.append(ScheduleGraph());selected=settings.workflows.count-1}
                let x=min(10000,max(-10000,(point.x-pan.x)/zoom)),y=min(10000,max(-10000,(point.y-pan.y)/zoom))
                mutate {g in if g.nodes.count<256 {let id=UUID().uuidString;g.nodes.append(ScheduleNode(id:id,title:route.model,routeId:route.id,x:x,y:y,task:.object(["kind":.string("extract"),"prompt":.string("分析输入材料，返回完整 JSON 报告。"),"max_output_tokens":.number(8192),"token_budget":.number(48000),"deadline_s":.number(300),"hard_deadline_s":.number(900)])));nodeId=id}}
                pool(route.id);return true
            }
        }
    }
    private func nodeCard(_ n:ScheduleNode)->some View {
        VStack(alignment:.leading,spacing:6) {
            Text(n.title).font(.headline).lineLimit(1)
            Text(routes.first {$0.id==n.routeId}?.model ?? n.nodeType).font(.caption).lineLimit(1)
            Text(n.tags.joined(separator:" · ")).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
        }.padding(12).frame(width:220,height:112,alignment:.leading)
        .background(.background,in:RoundedRectangle(cornerRadius:10))
        .overlay(RoundedRectangle(cornerRadius:10).stroke(nodeId==n.id ? Color.accentColor:Color.secondary,lineWidth:2))
        .overlay(alignment:.leading) {Circle().fill(Color.accentColor).frame(width:14,height:14).onTapGesture {connect(to:n.id)}}
        .overlay(alignment:.trailing) {Circle().fill(from==n.id ? Color.orange:Color.accentColor).frame(width:14,height:14).onTapGesture {from=n.id}}
        .onTapGesture {nodeId=n.id}
        .gesture(DragGesture(minimumDistance:4).onChanged {v in
            if origin==nil {origin=CGPoint(x:n.x,y:n.y)};guard let start=origin else {return}
            mutate {g in if let i=g.nodes.firstIndex(where:{$0.id==n.id}) {g.nodes[i].x=min(10000,max(-10000,start.x+v.translation.width/zoom));g.nodes[i].y=min(10000,max(-10000,start.y+v.translation.height/zoom))}}
        }.onEnded {_ in origin=nil})
        .dropDestination(for:String.self) {values,_ in guard let id=values.first,routes.contains(where:{$0.id==id}) else {return false};guard n.nodeType=="model" else {error="本地节点不绑定模型；请拖到空白处创建模型节点，或先套用模型职责模板";return true};mutate {g in if let i=g.nodes.firstIndex(where:{$0.id==n.id}) {g.nodes[i].routeId=id}};pool(id);return true}
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

    private func editor(_ i: Int, _ n: ScheduleNode) -> some View {
        VStack(alignment: .leading) {
            VStack(alignment:.leading,spacing:8) {
                TextField("节点名称", text: nodeBinding(i, \.title))
                TextField("分类标签，逗号分隔", text: Binding(get: { n.tags.joined(separator: ", ") }, set: { v in mutate { $0.nodes[i].tags = v.split(whereSeparator: { $0 == "," || $0 == "，" }).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty } } }))
                Button("↑") { moveNode(i, -1) }; Button("↓") { moveNode(i, 1) }
                Button("移除节点") { mutate { g in g.nodes.remove(at: i); g.edges.removeAll { $0.from == n.id || $0.to == n.id } }; nodeId = nil }
            }
            TextField("节点职责（可自由调整）", text: nodeBinding(i, \.role))
            if n.nodeType == "model" {
                Text("备用 / 补位：\(n.fallbackRoutes.map { id in routes.first { $0.id == id }?.model ?? id }.joined(separator: " → "))").font(.caption)
                Menu("从模型池添加备用") { ForEach(settings.pool.filter { $0.expiresAt > now && $0.id != n.routeId && !n.fallbackRoutes.contains($0.id) }) { p in Button(routes.first { $0.id == p.id }?.model ?? p.id) { mutate { g in if g.nodes[i].fallbackRoutes.count < 8 { g.nodes[i].fallbackRoutes.append(p.id) } } } } }
                Button("清空备用") { mutate { $0.nodes[i].fallbackRoutes = [] } }
            }
            LazyVGrid(columns: Array(repeating: GridItem(.flexible()), count: 2)) { ForEach(templates) { t in Button("套用 \(t.title)") { applyTemplate(t, to: i) } } }
            Text("绑定：\(routes.first { $0.id == n.routeId }.map { "\($0.source) / \($0.model)" } ?? n.routeId)").font(.caption).fixedSize(horizontal:false,vertical:true)
            ForEach(["kind", "prompt", "max_output_tokens", "token_budget", "deadline_s", "hard_deadline_s", "ttft_s", "idle_s", "output_mode", "report_max_chars"], id: \.self) { key in
                TextField(key, text: Binding(get: { if case .number(let v) = n.task[key] { return String(Int(v)) }; return n.task[key].text }, set: { v in mutate { g in if v.isEmpty && !["kind","prompt"].contains(key) {var fields=g.nodes[i].task.fields;fields.removeValue(forKey:key);g.nodes[i].task = .object(fields)} else {g.nodes[i].task[key] = ["kind", "prompt", "output_mode"].contains(key) ? .string(v) : (Double(v).map(SchedulerJSON.number) ?? .null)} } }))
            }
            Text("高级节点参数 JSON").font(.caption)
            TextEditor(text: $taskText).frame(height: 140).border(.quaternary)
            Button("应用高级参数") { do { let value = try JSONDecoder().decode(SchedulerJSON.self, from: Data(taskText.utf8)); guard case .object = value else { return }; mutate { $0.nodes[i].task = value } } catch { self.error = error.localizedDescription } }
        }
    }
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
    private func run() { guard let graph else { return }; do {
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
                sources = Array(Set(value["sources"].values.map { $0["id"].text } + routes.map(\.source))).filter { !$0.isEmpty }.sorted()
                runs = value["runs"].values
                templates = try value["templates"].decode([ScheduleTemplate].self)
                callPresets = value["call_presets"].values
                effectiveProfile = value["effective_profile"].pretty
                if workflowId.isEmpty { workflowId = value["supervisors"].values.first?["workflow_id"].text ?? "default" }
                if case .object = value["check"] { notice = value["check"].pretty }
            } else if value.fields.keys.contains("results") { report=value.pretty }
            else { if !value["run_id"].text.isEmpty { runId = value["run_id"].text }; detail = value.pretty; detailValue=value; finished = ["succeeded", "failed", "cancelled", "interrupted"].contains(value["state"].text) }
        } catch { self.error = error.localizedDescription }; busy = false }
    }
}
