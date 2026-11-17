// Paint the brush parity cases (tests/fixtures/brush_pts/cases.json) in
// Photoshop, one PNG per case, so iAi's brush can be measured against it.
//
// Run through scripts/brush_parity/run_ps.ps1, which passes three arguments:
//   0: path of cases.json   1: output folder   2: optional case-id filter
//
// Each case is painted with a plain computed round tip (no dynamics, texture,
// dual brush, noise, airbrush or smoothing) on a transparent layer of a new
// sRGB document at 72 ppi (so path points are pixels), by stroking a path
// with the Brush / Pencil tool and no simulated pressure. The user's own tool
// options are put back at the end.
#target photoshop

var cTID = function (s) { return charIDToTypeID(s); };
var sTID = function (s) { return stringIDToTypeID(s); };

var casesPath = arguments.length > 0 ? arguments[0] : "";
var outDir = arguments.length > 1 ? arguments[1] : "";
var only = arguments.length > 2 ? arguments[2] : "";
// Base pictures (`base` of a case) sit beside the cases file.
var baseDir = new File(casesPath).parent.fsName.replace(/\\/g, "/");

// One log per cases file: ps_paint.log for cases.json, ps_paint_<name>.log
// for the others.
var casesName = new File(casesPath).name.replace(/\.json$/, "");
var log = new File(outDir + (casesName == "cases" ? "/ps_paint.log" : "/ps_paint_" + casesName + ".log"));
log.encoding = "UTF-8";
log.open("w");

function readText(path) {
    var f = new File(path);
    f.encoding = "UTF-8";
    f.open("r");
    var t = f.read();
    f.close();
    return t;
}

function toolOptions(toolId) {
    var ref = new ActionReference();
    ref.putProperty(cTID("Prpr"), sTID("tool"));
    ref.putEnumerated(cTID("capp"), cTID("Ordn"), cTID("Trgt"));
    app.currentTool = toolId;
    return executeActionGet(ref).getObjectValue(sTID("currentToolOptions"));
}

function setToolOptions(toolId, opts) {
    var d = new ActionDescriptor();
    var r = new ActionReference();
    r.putClass(sTID(toolId));
    d.putReference(cTID("null"), r);
    d.putObject(cTID("T   "), sTID("currentToolOptions"), opts);
    executeAction(cTID("setd"), d, DialogModes.NO);
}

// Dodge / Burn: the Range menu's enumeration, read off the tool itself.
var RANGE_CODE = { shadows: "S", midtones: "M", highlights: "H" };

function plainOptions(c) {
    var o = new ActionDescriptor();
    var b = new ActionDescriptor();
    b.putUnitDouble(sTID("diameter"), sTID("pixelsUnit"), c.size);
    b.putUnitDouble(sTID("hardness"), sTID("percentUnit"), c.hardness);
    b.putUnitDouble(sTID("angle"), sTID("angleUnit"), 0);
    b.putUnitDouble(sTID("roundness"), sTID("percentUnit"), 100);
    b.putUnitDouble(sTID("spacing"), sTID("percentUnit"), c.spacing);
    b.putBoolean(sTID("interfaceIconFrameDimmed"), true);
    b.putBoolean(sTID("flipX"), false);
    b.putBoolean(sTID("flipY"), false);
    o.putObject(sTID("brush"), sTID("computedBrush"), b);
    if (c.tool == "dodge" || c.tool == "burn") {
        var toolId = c.tool == "dodge" ? "dodgeTool" : "burnInTool";
        var cur = toolOptions(toolId);
        var enumType = cur.getEnumerationType(sTID("mode"));
        var code = (c.tool == "dodge" ? "dodge" : "burnIn") + RANGE_CODE[c.range];
        o.putEnumerated(sTID("mode"), enumType, sTID(code));
        o.putInteger(sTID("exposure"), c.exposure);
        // Protect Tones is the opposite of the legacy (pre-CS4) maths.
        o.putBoolean(sTID("useLegacy"), !c.protect);
    } else {
        o.putInteger(sTID("opacity"), c.opacity);
        o.putInteger(sTID("flow"), c.flow);
        o.putEnumerated(sTID("mode"), sTID("blendModel"), sTID("normal"));
    }
    var off = ["useTipDynamics", "useScatter", "useTexture", "useColorDynamics",
        "wetEdges", "noise", "usePaintDynamics", "useBrushPose", "repeat",
        "smoothing", "usePressureOverridesSize", "usePressureOverridesOpacity",
        "autoErase"];
    for (var i = 0; i < off.length; i++) o.putBoolean(sTID(off[i]), false);
    o.putDouble(sTID("smoothingValue"), 0);
    var dual = new ActionDescriptor();
    dual.putBoolean(sTID("useDualBrush"), false);
    o.putObject(sTID("dualBrush"), sTID("dualBrush"), dual);
    return o;
}

function describe(o) {
    var b = o.getObjectValue(sTID("brush"));
    var s = "brush=" + typeIDToStringID(o.getObjectType(sTID("brush")));
    var keys = ["diameter", "hardness", "spacing"];
    for (var i = 0; i < keys.length; i++) {
        if (b.hasKey(sTID(keys[i]))) s += " " + keys[i] + "=" + b.getUnitDoubleValue(sTID(keys[i]));
    }
    var ints = ["opacity", "flow", "exposure"];
    for (var j = 0; j < ints.length; j++) {
        if (o.hasKey(sTID(ints[j]))) s += " " + ints[j] + "=" + o.getInteger(sTID(ints[j]));
    }
    var bools = ["useTipDynamics", "useTexture", "noise", "repeat", "smoothing", "wetEdges", "usePaintDynamics", "useScatter"];
    for (var k = 0; k < bools.length; k++) {
        if (o.hasKey(sTID(bools[k])) && o.getBoolean(sTID(bools[k]))) s += " ON:" + bools[k];
    }
    if (o.hasKey(sTID("dualBrush")) && o.getObjectValue(sTID("dualBrush")).getBoolean(sTID("useDualBrush"))) s += " ON:dualBrush";
    if (o.hasKey(sTID("mode"))) s += " mode=" + typeIDToStringID(o.getEnumerationValue(sTID("mode")));
    if (o.hasKey(sTID("useLegacy"))) s += " legacy=" + o.getBoolean(sTID("useLegacy"));
    return s;
}

function strokePoints(doc, pts, toolType) {
    var arr = [];
    var src = pts;
    if (src.length == 1) {
        // A one-point sub-path is refused; a 0.001 px segment paints the
        // press dab and nothing more (the next dab is a spacing away).
        src = [pts[0], [pts[0][0] + 0.001, pts[0][1]]];
    }
    for (var i = 0; i < src.length; i++) {
        var p = new PathPointInfo();
        p.kind = PointKind.CORNERPOINT;
        p.anchor = [src[i][0], src[i][1]];
        p.leftDirection = p.anchor;
        p.rightDirection = p.anchor;
        arr.push(p);
    }
    var sp = new SubPathInfo();
    sp.operation = ShapeOperation.SHAPEXOR;
    sp.closed = false;
    sp.entireSubPath = arr;
    var path = doc.pathItems.add("iai_parity", [sp]);
    path.strokePath(toolType, false);
    path.remove();
}

function paintCase(c) {
    var bits = c.bits == 16 ? BitsPerChannelType.SIXTEEN : BitsPerChannelType.EIGHT;
    var doc;
    if (c.base) {
        // The tool works over a picture (Dodge / Burn): open it, at 72 ppi.
        doc = app.open(new File(baseDir + "/" + c.base));
        doc.resizeImage(undefined, undefined, 72, ResampleMethod.NONE);
    } else {
        doc = app.documents.add(UnitValue(c.w, "px"), UnitValue(c.h, "px"), 72, c.id,
            NewDocumentMode.RGB, DocumentFill.TRANSPARENT, 1, bits, "sRGB IEC61966-2.1");
    }
    var col = new SolidColor();
    col.rgb.red = c.color[0];
    col.rgb.green = c.color[1];
    col.rgb.blue = c.color[2];
    app.foregroundColor = col;
    var toolId = { pencil: "pencilTool", dodge: "dodgeTool", burn: "burnInTool" }[c.tool] || "paintbrushTool";
    var toolType = { pencil: ToolType.PENCIL, dodge: ToolType.DODGE, burn: ToolType.BURN }[c.tool] || ToolType.BRUSH;
    app.currentTool = toolId;
    setToolOptions(toolId, plainOptions(c));
    log.writeln(c.id + ": " + describe(toolOptions(toolId)));
    for (var s = 0; s < c.strokes.length; s++) strokePoints(doc, c.strokes[s], toolType);
    var png = new PNGSaveOptions();
    png.compression = 1;
    png.interlaced = false;
    doc.saveAs(new File(outDir + "/" + c.id + ".png"), png, true, Extension.LOWERCASE);
    doc.close(SaveOptions.DONOTSAVECHANGES);
}

var savedUnits = app.preferences.rulerUnits;
var savedDialogs = app.displayDialogs;
var savedFg = app.foregroundColor;
var savedTool = app.currentTool;
var saved = {};
var result = "ok";
try {
    app.displayDialogs = DialogModes.NO;
    app.preferences.rulerUnits = Units.PIXELS;
    saved.paintbrushTool = toolOptions("paintbrushTool");
    saved.pencilTool = toolOptions("pencilTool");
    saved.dodgeTool = toolOptions("dodgeTool");
    saved.burnInTool = toolOptions("burnInTool");
    var spec = eval("(" + readText(casesPath) + ")");
    var n = 0;
    for (var i = 0; i < spec.cases.length; i++) {
        var c = spec.cases[i];
        if (only != "" && c.id.indexOf(only) != 0) continue;
        if (c.spacing === undefined) c.spacing = 25;
        if (c.opacity === undefined) c.opacity = 100;
        if (c.flow === undefined) c.flow = 100;
        if (c.hardness === undefined) c.hardness = 100;
        if (c.color === undefined) c.color = [0, 0, 0];
        try {
            paintCase(c);
            n++;
        } catch (e) {
            log.writeln(c.id + ": ERROR " + e + " (line " + e.line + ")");
            result = "error";
            while (app.documents.length > 0 && app.activeDocument.name.indexOf(c.id) == 0) {
                app.activeDocument.close(SaveOptions.DONOTSAVECHANGES);
            }
        }
    }
    log.writeln("painted " + n);
} catch (e) {
    log.writeln("FATAL " + e + " (line " + e.line + ")");
    result = "fatal";
} finally {
    try { if (saved.paintbrushTool) setToolOptions("paintbrushTool", saved.paintbrushTool); } catch (e) { log.writeln("restore brush: " + e); }
    try { if (saved.pencilTool) setToolOptions("pencilTool", saved.pencilTool); } catch (e) { log.writeln("restore pencil: " + e); }
    try { if (saved.dodgeTool) setToolOptions("dodgeTool", saved.dodgeTool); } catch (e) { log.writeln("restore dodge: " + e); }
    try { if (saved.burnInTool) setToolOptions("burnInTool", saved.burnInTool); } catch (e) { log.writeln("restore burn: " + e); }
    try { app.currentTool = savedTool; } catch (e) {}
    app.foregroundColor = savedFg;
    app.preferences.rulerUnits = savedUnits;
    app.displayDialogs = savedDialogs;
    log.close();
}
result;
