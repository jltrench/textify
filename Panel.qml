import QtQuick
import QtQuick.Controls
import QtQuick.Effects
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

Panel {
  id: root
  moduleName: "jltrench.textify"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null

  readonly property string binPath: Qt.resolvedUrl("bin/textify").toString().replace("file://", "")

  property bool busy: false
  property string statusText: "Choose a capture mode"
  property string statusTone: "neutral"
  property string pendingCopyText: ""
  property string lastText: ""
  property string lastLang: ""
  property bool lastCopied: false
  property double lastConfidence: 0
  property var history: []
  readonly property int maxHistory: 12
  property string activeLang: "" // "" = auto-detect from keyboard layout
  property string detectedLayout: ""
  property string detectedLang: ""
  property var installedLangs: []
  property int cursorIndex: 0

  readonly property int maxProcessOutputChars: 128 * 1024
  readonly property int maxTextChars: 32 * 1024
  readonly property int maxFieldChars: 256
  readonly property int maxDiagnosticChars: 512
  readonly property int maxLanguageCount: 128
  readonly property int ocrTimeoutMs: 420000
  readonly property int shortProcessTimeoutMs: 20000
  readonly property int copyTimeoutMs: 15000

  readonly property color ink: root.barForeground
  readonly property color secondaryInk: Util.alpha(root.ink, 0.68)
  readonly property color quietInk: Util.alpha(root.ink, 0.52)
  readonly property color hairline: Util.alpha(root.ink, 0.12)
  readonly property color statusColor: {
    if (root.statusTone === "error" || root.statusTone === "warning") return Color.urgent
    if (root.statusTone === "success") return Color.accent
    if (root.statusTone === "busy") return root.ink
    return root.quietInk
  }

  function open(payload) {
    root.controller.show()
  }

  function close() {
    root.controller.hide()
  }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.hostWidget || root, direction)
    return false
  }

  function setStatus(text, tone) {
    root.statusText = text
    root.statusTone = tone || "neutral"
  }

  function boundedText(value, limit) {
    var text = String(value || "")
    return text.length > limit ? text.slice(0, limit) : text
  }

  function collectProcessLine(process, line) {
    if (process.outputTooLarge) return
    var value = String(line || "") + "\n"
    if (process.output.length + value.length > root.maxProcessOutputChars) {
      process.outputTooLarge = true
      if (process.running) process.signal(9)
      return
    }
    process.output += value
  }

  function logDiagnostic(prefix, line) {
    var diagnostic = root.boundedText(String(line || "").trim(), root.maxDiagnosticChars)
    if (diagnostic !== "") console.warn(prefix + ":", diagnostic)
  }

  function languageName(code) {
    var names = {
      eng: "English",
      por: "Português",
      spa: "Español",
      fra: "Français",
      deu: "Deutsch",
      ita: "Italiano",
      nld: "Nederlands",
      rus: "Русский",
      jpn: "日本語",
      chi_sim: "简体中文",
      chi_tra: "繁體中文",
      kor: "한국어",
      ara: "العربية"
    }
    return names[code] || String(code).toUpperCase()
  }

  function languageOptions() {
    var options = [{
      value: "",
      label: root.detectedLang === ""
        ? "Auto detect"
        : "Auto · " + root.detectedLang.toUpperCase()
    }]
    for (var i = 0; i < root.installedLangs.length; i++) {
      var code = String(root.installedLangs[i])
      if (code === "osd") continue
      options.push({ value: code, label: root.languageName(code) })
    }
    return options
  }

  function run(mode) {
    if (root.busy) return
    root.busy = true
    root.setStatus(mode === "region" ? "Select an area on screen" : "Reading the screen", "busy")
    ocrProc.mode = mode
    var args = [root.binPath, mode, "--json"]
    if (root.activeLang !== "") args.push("--lang", root.activeLang)
    ocrProc.command = args
    ocrProc.running = true
  }

  function copyText(text) {
    if (!text || copyProc.running) return
    var value = String(text)
    if (value.length > root.maxTextChars) {
      root.setStatus("Text is too large to copy", "error")
      return
    }
    root.pendingCopyText = value
    root.setStatus("Copying text", "busy")
    copyProc.stdinEnabled = true
    copyProc.command = [root.binPath, "copy"]
    copyProc.running = true
  }

  function clearResult() {
    root.lastText = ""
    root.lastLang = ""
    root.lastCopied = false
    root.setStatus("Choose a capture mode", "neutral")
    root.normalizeCursor()
  }

  function clearHistory() {
    root.history = []
    root.normalizeCursor()
  }

  function removeHistory(index) {
    var h = root.history.slice()
    h.splice(index, 1)
    root.history = h
    root.normalizeCursor()
  }

  function pushHistory(entry) {
    var h = root.history.slice()
    entry.text = root.boundedText(entry.text, root.maxTextChars)
    entry.lang = root.boundedText(entry.lang, root.maxFieldChars)
    entry.when = root.boundedText(entry.when, root.maxFieldChars)
    h.unshift(entry)
    if (h.length > root.maxHistory) h = h.slice(0, root.maxHistory)
    root.history = h
  }

  // The panel owns one simple cursor model so mouse, arrows, and j/k all
  // describe the same active target. This keeps the small popup usable
  // without a mouse while avoiding a forest of independent focus rings.
  function focusItems() {
    var items = [
      { kind: "region", item: regionButton },
      { kind: "full", item: fullButton },
      { kind: "language", item: languageSelect }
    ]
    if (root.lastText !== "") {
      items.push({ kind: "copy", item: copyButton })
      items.push({ kind: "clearResult", item: clearResultAction })
    }
    for (var i = 0; i < root.history.length; i++) {
      items.push({ kind: "history", index: i, item: historyRepeater.itemAt(i) })
    }
    if (root.history.length > 0)
      items.push({ kind: "clearHistory", item: clearHistoryButton })
    return items
  }

  function cursorIs(kind, index) {
    var items = root.focusItems()
    if (root.cursorIndex < 0 || root.cursorIndex >= items.length) return false
    var entry = items[root.cursorIndex]
    return entry.kind === kind && (index === undefined || entry.index === index)
  }

  function normalizeCursor() {
    var count = root.focusItems().length
    root.cursorIndex = count === 0 ? 0 : Math.max(0, Math.min(root.cursorIndex, count - 1))
  }

  function setCursorTo(kind, index) {
    var items = root.focusItems()
    for (var i = 0; i < items.length; i++) {
      if (items[i].kind === kind && (index === undefined || items[i].index === index)) {
        root.cursorIndex = i
        root.ensureCursorVisible(items[i].item)
        return
      }
    }
  }

  function moveCursor(dx, dy) {
    var items = root.focusItems()
    if (items.length === 0) return
    var delta = dx !== 0 ? dx : dy
    if (delta === 0) return
    root.cursorIndex = Math.max(0, Math.min(items.length - 1, root.cursorIndex + delta))
    root.ensureCursorVisible(items[root.cursorIndex].item)
  }

  function ensureCursorVisible(item) {
    if (!item || !contentFlick || contentFlick.height <= 0) return
    var point = item.mapToItem(contentFlick.contentItem, 0, 0)
    var top = point.y
    var bottom = top + item.height
    var viewportTop = contentFlick.contentY
    var viewportBottom = viewportTop + contentFlick.height
    if (top < viewportTop) contentFlick.contentY = top
    else if (bottom > viewportBottom) contentFlick.contentY = bottom - contentFlick.height
  }

  function activateCursor() {
    var items = root.focusItems()
    if (root.cursorIndex < 0 || root.cursorIndex >= items.length) return
    var entry = items[root.cursorIndex]
    if (entry.kind === "region") root.run("region")
    else if (entry.kind === "full") root.run("full")
    else if (entry.kind === "language") languageSelect.toggle()
    else if (entry.kind === "copy") root.copyText(root.lastText)
    else if (entry.kind === "clearResult") root.clearResult()
    else if (entry.kind === "history") root.copyText(root.history[entry.index].text)
    else if (entry.kind === "clearHistory") root.clearHistory()
  }

  onOpenedChanged: {
    if (root.opened) {
      root.cursorIndex = 0
      contentFlick.contentY = 0
      // Refresh detected layout + installed languages.
      langProc.command = [root.binPath, "lang"]
      langProc.running = true
      langsProc.command = [root.binPath, "langs"]
      langsProc.running = true
    }
  }

  Process {
    id: langProc

    property string output: ""
    property bool outputTooLarge: false
    property bool timedOut: false

    stdout: SplitParser {
      splitMarker: "\n"
      onRead: function(line) { root.collectProcessLine(langProc, line) }
    }

    stderr: SplitParser {
      splitMarker: "\n"
      onRead: function(line) { root.logDiagnostic("Textify language", line) }
    }

    onStarted: {
      langProc.output = ""
      langProc.outputTooLarge = false
      langProc.timedOut = false
      langTimeout.restart()
    }

    onExited: {
      langTimeout.stop()
      if (langProc.timedOut || langProc.outputTooLarge) {
        root.detectedLayout = ""
        root.detectedLang = ""
        return
      }
      try {
        var parsed = JSON.parse(langProc.output.trim())
        root.detectedLayout = root.boundedText(parsed.layout || "", root.maxFieldChars)
        root.detectedLang = root.boundedText(parsed.lang || "", root.maxFieldChars)
      } catch (e) {
        root.detectedLayout = ""
        root.detectedLang = ""
      }
    }
  }

  Timer {
    id: langTimeout
    interval: root.shortProcessTimeoutMs
    repeat: false
    onTriggered: {
      if (langProc.running) {
        langProc.timedOut = true
        langProc.signal(9)
      }
    }
  }

  Process {
    id: ocrProc

    property string mode: "region"
    property string output: ""
    property bool outputTooLarge: false
    property bool timedOut: false

    onStarted: {
      root.busy = true
      ocrProc.output = ""
      ocrProc.outputTooLarge = false
      ocrProc.timedOut = false
      ocrTimeout.restart()
    }

    stdout: SplitParser {
      splitMarker: "\n"
      onRead: function(line) { root.collectProcessLine(ocrProc, line) }
    }

    stderr: SplitParser {
      splitMarker: "\n"
      onRead: function(line) { root.logDiagnostic("Textify OCR", line) }
    }

    onExited: function(exitCode) {
      ocrTimeout.stop()
      root.busy = false
      if (ocrProc.timedOut) {
        root.setStatus("OCR timed out · try a smaller capture", "error")
        return
      }
      if (ocrProc.outputTooLarge) {
        root.setStatus("OCR result exceeded the safe limit", "error")
        return
      }

      var raw = ocrProc.output.trim()
      var parsed
      try {
        parsed = JSON.parse(raw)
      } catch (e) {
        root.setStatus("OCR could not return a result", "error")
        if (raw !== "") console.error("Textify OCR output:", root.boundedText(raw, root.maxDiagnosticChars))
        return
      }

      if (parsed.error) {
        root.setStatus(root.boundedText(parsed.error, root.maxFieldChars), "error")
        return
      }
      if (exitCode !== 0) {
        root.setStatus("Capture failed · check the OCR tools", "error")
        return
      }

      var resultText = String(parsed.text || "")
      if (resultText.length > root.maxTextChars) {
        root.setStatus("OCR result exceeded the safe limit", "error")
        return
      }
      root.lastText = resultText
      root.lastLang = root.boundedText(parsed.lang || "", root.maxFieldChars)
      root.lastCopied = parsed.copied === true
      var confidence = Number(parsed.confidence)
      root.lastConfidence = isFinite(confidence) ? Math.max(0, Math.min(100, confidence)) : 0
      if (root.lastText.trim() === "") {
        root.setStatus("No text found in that capture", "warning")
      } else if (root.lastConfidence < 40) {
        // Low confidence: the region was likely icons/graphics, not text.
        root.setStatus("Low confidence · the capture may contain graphics", "warning")
      } else {
        root.setStatus(root.lastCopied ? "Copied to clipboard" : "Text ready to copy", "success")
        root.pushHistory({
          text: root.lastText,
          lang: root.lastLang,
          when: new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
        })
      }
    }
  }

  Timer {
    id: ocrTimeout
    interval: root.ocrTimeoutMs
    repeat: false
    onTriggered: {
      if (ocrProc.running) {
        ocrProc.timedOut = true
        ocrProc.signal(9)
      }
    }
  }

  Process {
    id: langsProc

    property string output: ""
    property bool outputTooLarge: false
    property bool timedOut: false

    stdout: SplitParser {
      splitMarker: "\n"
      onRead: function(line) { root.collectProcessLine(langsProc, line) }
    }

    stderr: SplitParser {
      splitMarker: "\n"
      onRead: function(line) { root.logDiagnostic("Textify languages", line) }
    }

    onStarted: {
      langsProc.output = ""
      langsProc.outputTooLarge = false
      langsProc.timedOut = false
      langsTimeout.restart()
    }

    onExited: {
      langsTimeout.stop()
      if (langsProc.timedOut || langsProc.outputTooLarge) {
        root.installedLangs = []
        return
      }
      try {
        var parsed = JSON.parse(langsProc.output.trim())
        if (!Array.isArray(parsed) || parsed.length > root.maxLanguageCount) {
          root.installedLangs = []
          return
        }
        var safe = []
        for (var i = 0; i < parsed.length; i++) {
          var code = String(parsed[i] || "")
          if (code.length === 0 || code.length > root.maxFieldChars) {
            root.installedLangs = []
            return
          }
          safe.push(code)
        }
        root.installedLangs = safe
      } catch (e) {
        root.installedLangs = []
      }
    }
  }

  Timer {
    id: langsTimeout
    interval: root.shortProcessTimeoutMs
    repeat: false
    onTriggered: {
      if (langsProc.running) {
        langsProc.timedOut = true
        langsProc.signal(9)
      }
    }
  }

  Process {
    id: copyProc

    stdinEnabled: true
    property string output: ""
    property bool outputTooLarge: false
    property bool timedOut: false

    stdout: SplitParser {
      splitMarker: "\n"
      onRead: function(line) { root.collectProcessLine(copyProc, line) }
    }

    stderr: SplitParser {
      splitMarker: "\n"
      onRead: function(line) { root.logDiagnostic("Textify copy", line) }
    }

    onStarted: {
      copyProc.output = ""
      copyProc.outputTooLarge = false
      copyProc.timedOut = false
      copyTimeout.restart()
      copyProc.write(root.pendingCopyText)
      Qt.callLater(function() {
        if (copyProc.running) copyProc.stdinEnabled = false
      })
    }

    onExited: function(exitCode) {
      copyTimeout.stop()
      var copiedLatest = root.pendingCopyText === root.lastText
      root.pendingCopyText = ""
      if (copyProc.timedOut || copyProc.outputTooLarge) {
        root.setStatus("Copy timed out · text was not sent", "error")
        return
      }
      if (exitCode === 0) {
        if (copiedLatest) root.lastCopied = true
        root.setStatus("Copied to clipboard", "success")
      } else {
        root.setStatus("Could not copy the text", "error")
      }
    }
  }

  Timer {
    id: copyTimeout
    interval: root.copyTimeoutMs
    repeat: false
    onTriggered: {
      if (copyProc.running) {
        copyProc.timedOut = true
        copyProc.signal(9)
      }
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.hostWidget || root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(420))
    contentHeight: panel.fittedContentHeight(contentColumn.implicitHeight, Style.space(640))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      blocked: languageSelect.popupOpen
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onMoveRequested: function(dx, dy) { root.moveCursor(dx, dy) }
      onActivateRequested: root.activateCursor()

      Flickable {
        id: contentFlick
        anchors.fill: parent
        contentWidth: width
        contentHeight: contentColumn.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        interactive: contentHeight > height
        ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

        Column {
          id: contentColumn
          width: contentFlick.width
          spacing: Style.space(16)

          Row {
            id: hero
            width: parent.width
            spacing: Style.space(12)
            height: Math.max(Style.space(44), languageSelect.implicitHeight)

            BorderSurface {
              id: iconMark
              width: Style.space(44)
              height: width
              anchors.verticalCenter: parent.verticalCenter
              color: Util.alpha(root.ink, 0.08)
              borderSpec: Border.flat(root.hairline, Style.space(1))
              radius: Style.cornerRadius

              Image {
                id: heroIcon
                anchors.centerIn: parent
                width: Style.space(27)
                height: width
                source: Qt.resolvedUrl("icon.svg")
                sourceSize: Qt.size(Math.round(width * Screen.devicePixelRatio),
                                    Math.round(height * Screen.devicePixelRatio))
                fillMode: Image.PreserveAspectFit
                asynchronous: true
                smooth: true
                visible: false
              }

              MultiEffect {
                anchors.fill: heroIcon
                source: heroIcon
                colorization: 1.0
                colorizationColor: root.ink
              }
            }

            Column {
              id: heroCopy
              width: Math.max(0, parent.width - iconMark.width - languageSelect.width - parent.spacing * 2)
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(3)

              Text {
                width: parent.width
                text: "Textify"
                color: root.ink
                font.family: root.bar ? root.bar.fontFamily : Style.font.family
                font.pixelSize: Style.font.heading
                font.weight: Font.Medium
              }

              Text {
                width: parent.width
                text: "Private OCR for anything on screen"
                color: root.secondaryInk
                font.family: root.bar ? root.bar.fontFamily : Style.font.family
                font.pixelSize: Style.font.bodySmall
                elide: Text.ElideRight
              }
            }

            Dropdown {
              id: languageSelect
              width: Style.space(126)
              anchors.verticalCenter: parent.verticalCenter
              Accessible.role: Accessible.Button
              Accessible.name: "OCR language"
              label: "Language"
              value: root.activeLang
              options: root.languageOptions()
              enabled: !root.busy && root.installedLangs.length > 0
              foreground: root.ink
              hasCursor: root.cursorIs("language")
              onChanged: root.activeLang = value
              onHovered: function(isHovered) {
                if (isHovered) root.setCursorTo("language")
              }
            }
          }

          Column {
            id: captureSection
            width: parent.width
            spacing: Style.space(8)

            Text {
              width: parent.width
              text: "Capture"
              color: root.ink
              font.family: root.bar ? root.bar.fontFamily : Style.font.family
              font.pixelSize: Style.font.title
              font.weight: Font.Medium
            }

            Text {
              width: parent.width
              text: "Choose the smallest area that contains the text you need."
              color: root.secondaryInk
              font.family: root.bar ? root.bar.fontFamily : Style.font.family
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }

            Row {
              width: parent.width
              spacing: Style.space(8)

              Button {
                id: regionButton
                width: (parent.width - parent.spacing) / 2
                height: Style.space(42)
                Accessible.role: Accessible.Button
                Accessible.name: "Select region"
                Accessible.description: "Select an area of the screen to read"
                text: "Select region"
                tooltipText: "Select an area of the screen"
                enabled: !root.busy
                bordered: true
                background: Style.normalFill
                foreground: root.ink
                accent: root.ink
                hasCursor: root.cursorIs("region")
                onClicked: root.run("region")
                onHovered: function(isHovered) {
                  if (isHovered) root.setCursorTo("region")
                }
              }

              Button {
                id: fullButton
                width: (parent.width - parent.spacing) / 2
                height: Style.space(42)
                Accessible.role: Accessible.Button
                Accessible.name: "Full screen"
                Accessible.description: "Read all visible text"
                text: "Full screen"
                tooltipText: "Read all visible text"
                enabled: !root.busy
                bordered: true
                foreground: root.ink
                accent: root.ink
                hasCursor: root.cursorIs("full")
                onClicked: root.run("full")
                onHovered: function(isHovered) {
                  if (isHovered) root.setCursorTo("full")
                }
              }
            }

            Text {
              width: parent.width
              text: "Region is best for a window, paragraph, or label."
              color: root.quietInk
              font.family: root.bar ? root.bar.fontFamily : Style.font.family
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }
          }

          Row {
            id: statusRow
            width: parent.width
            spacing: Style.space(8)

            Rectangle {
              id: statusDot
              width: Style.space(7)
              height: width
              radius: width / 2
              anchors.top: statusTextItem.top
              anchors.topMargin: Style.space(4)
              color: root.statusColor
              opacity: root.busy ? 0.72 : 1.0
            }

            Text {
              id: statusTextItem
              width: parent.width - statusDot.width - parent.spacing
              text: root.statusText
              color: root.statusTone === "neutral" ? root.secondaryInk : root.statusColor
              font.family: root.bar ? root.bar.fontFamily : Style.font.family
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
              lineHeight: 1.2
            }
          }

          Column {
            id: resultSection
            visible: root.lastText !== ""
            width: parent.width
            spacing: Style.space(8)

            Row {
              width: parent.width
              height: clearResultAction.implicitHeight

              Text {
                width: parent.width - clearResultAction.width - parent.spacing
                anchors.verticalCenter: parent.verticalCenter
                text: "Latest capture"
                color: root.ink
                font.family: root.bar ? root.bar.fontFamily : Style.font.family
                font.pixelSize: Style.font.title
                font.weight: Font.Medium
              }

              PanelActionButton {
                id: clearResultAction
                size: Style.space(30)
                Accessible.role: Accessible.Button
                Accessible.name: "Clear latest capture"
                iconText: "󰅖"
                tooltipText: "Clear latest capture"
                foreground: root.ink
                hasCursor: root.cursorIs("clearResult")
                onClicked: root.clearResult()
                onHovered: function(isHovered) {
                  if (isHovered) root.setCursorTo("clearResult")
                }
              }
            }

            BorderSurface {
              id: resultSurface
              width: parent.width
              height: Style.space(168)
              color: Util.alpha(root.ink, 0.045)
              borderSpec: Border.flat(root.hairline, Style.space(1))
              radius: Style.cornerRadius

              Flickable {
                id: resultFlick
                anchors.fill: parent
                anchors.margins: Style.space(12)
                contentWidth: width
                contentHeight: resultText.implicitHeight
                clip: true
                boundsBehavior: Flickable.StopAtBounds
                ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

                Text {
                  id: resultText
                  width: resultFlick.width
                  text: root.lastText
                  textFormat: Text.PlainText
                  color: root.ink
                  font.family: root.bar ? root.bar.fontFamily : Style.font.family
                  font.pixelSize: Style.font.body
                  lineHeight: 1.35
                  wrapMode: Text.Wrap
                }
              }
            }

            Text {
              width: parent.width
              text: {
                var confidence = Math.round(root.lastConfidence) + "% confidence"
                var language = root.lastLang === "" ? "" : root.languageName(root.lastLang)
                var prefix = root.lastCopied ? "Copied automatically" : "Ready to copy"
                return prefix + " · " + confidence + (language === "" ? "" : " · " + language)
              }
              color: root.secondaryInk
              font.family: root.bar ? root.bar.fontFamily : Style.font.family
              font.pixelSize: Style.font.caption
              elide: Text.ElideRight
            }

            Button {
              id: copyButton
              width: parent.width
              height: Style.space(40)
              Accessible.role: Accessible.Button
              Accessible.name: "Copy latest extraction"
              text: "Copy again"
              tooltipText: "Copy the latest extraction"
              enabled: root.lastText !== "" && !root.busy
              bordered: true
              background: Style.normalFill
              foreground: root.ink
              accent: root.ink
              hasCursor: root.cursorIs("copy")
              onClicked: root.copyText(root.lastText)
              onHovered: function(isHovered) {
                if (isHovered) root.setCursorTo("copy")
              }
            }
          }

          Column {
            id: historySection
            visible: root.history.length > 0
            width: parent.width
            spacing: Style.space(8)

            Row {
              width: parent.width
              height: clearHistoryButton.height

              Text {
                width: parent.width - clearHistoryButton.width - parent.spacing
                anchors.verticalCenter: parent.verticalCenter
                text: "History"
                color: root.ink
                font.family: root.bar ? root.bar.fontFamily : Style.font.family
                font.pixelSize: Style.font.title
                font.weight: Font.Medium
              }

              Button {
                id: clearHistoryButton
                width: Style.space(96)
                height: Style.space(30)
                Accessible.role: Accessible.Button
                Accessible.name: "Clear history"
                text: "Clear history"
                tooltipText: "Remove all saved captures"
                bordered: true
                foreground: root.ink
                accent: root.ink
                hasCursor: root.cursorIs("clearHistory")
                onClicked: root.clearHistory()
                onHovered: function(isHovered) {
                  if (isHovered) root.setCursorTo("clearHistory")
                }
              }
            }

            Column {
              id: historyColumn
              width: parent.width
              spacing: Style.space(4)

              Repeater {
                id: historyRepeater
                model: root.history

                delegate: CursorSurface {
                  id: historyRow
                  required property var modelData
                  required property int index
                  Accessible.role: Accessible.Button
                  Accessible.name: "Copy capture: " + root.boundedText(String(modelData.text || "").replace(/\s+/g, " ").trim(), root.maxFieldChars)
                  Accessible.description: "Copy this saved capture"
                  width: historyColumn.width
                  height: Style.space(52)
                  foreground: root.ink
                  accent: root.ink
                  hasCursor: root.cursorIs("history", index)

                  HoverHandler {
                    cursorShape: Qt.PointingHandCursor
                    onHoveredChanged: {
                      if (hovered) root.setCursorTo("history", index)
                    }
                  }

                  Column {
                    anchors.left: parent.left
                    anchors.leftMargin: Style.space(12)
                    anchors.right: removeHistoryAction.left
                    anchors.rightMargin: Style.space(8)
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Style.space(2)

                    Text {
                      width: parent.width
                      text: {
                        var oneLine = String(modelData.text || "").replace(/\s+/g, " ").trim()
                        return oneLine.length > 58 ? oneLine.slice(0, 58) + "..." : oneLine
                      }
                      textFormat: Text.PlainText
                      color: root.ink
                      font.family: root.bar ? root.bar.fontFamily : Style.font.family
                      font.pixelSize: Style.font.bodySmall
                      elide: Text.ElideRight
                    }

                    Text {
                      width: parent.width
                      text: (modelData.lang ? root.languageName(modelData.lang) : "Auto") +
                        " · " + modelData.when
                      color: root.quietInk
                      font.family: root.bar ? root.bar.fontFamily : Style.font.family
                      font.pixelSize: Style.font.caption
                      elide: Text.ElideRight
                    }
                  }

                  PanelActionButton {
                    id: removeHistoryAction
                    anchors.right: parent.right
                    anchors.rightMargin: Style.space(8)
                    anchors.verticalCenter: parent.verticalCenter
                    size: Style.space(30)
                    iconText: "󰅖"
                    tooltipText: "Remove this capture"
                    foreground: root.ink
                    onClicked: root.removeHistory(index)
                  }

                  TapHandler {
                    onTapped: root.copyText(modelData.text)
                  }
                }
              }
            }
          }

          Text {
            width: parent.width
            text: "Runs locally · no screenshots or text leave this device"
            color: root.quietInk
            font.family: root.bar ? root.bar.fontFamily : Style.font.family
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }
        }
      }
    }
  }
}
