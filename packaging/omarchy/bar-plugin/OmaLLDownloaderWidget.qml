import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import qs.Ui

// OmaLLDownloader in the bar: a download glyph, how many tasks are running and their
// combined speed. The popup lists each task with its own progress line.
//
// It reads `omalld tasks --json` — the same record the window and the CLI use — so there is
// one source of truth and no second daemon.
Panel {
  id: root
  moduleName: "dimdoremy.omalldownloader"
  // The bar exists once per monitor, so every instance would register the same IPC target
  // and only the first would answer. Each instance answers on its own screen instead:
  //   omarchy-shell dimdoremy.omalldownloader.HDMI-A-1 toggle
  manageIpc: false
  readonly property string screenName: {
    var win = root.QsWindow && root.QsWindow.window
    return win && win.screen ? win.screen.name : "default"
  }

  readonly property string glyph: "\uf019"   // nf-fa-download
  readonly property string cmd: String(setting("omalldPath", "omalld"))
  readonly property int refreshMs: Math.max(1, Number(setting("refreshSeconds", 2)) || 2) * 1000
  readonly property string labelMode: String(setting("label", "speed"))

  // The bar sizes a widget from its implicit size; the button fills that.
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  property var tasks: []
  property bool loaded: false

  readonly property var running: tasks.filter(function (t) { return t.status === "running" || t.status === "queued" })
  readonly property var failed: tasks.filter(function (t) { return t.status === "failed" })
  readonly property real speed: running.reduce(function (sum, t) { return sum + (Number(t.speed) || 0) }, 0)
  readonly property bool busy: running.length > 0

  readonly property string barText: {
    if (!root.loaded) return glyph
    if (!root.busy && root.failed.length > 0) return glyph + "  !"
    if (!root.busy) return glyph
    if (root.bar && root.bar.vertical) return glyph
    if (root.labelMode === "count") return glyph + "  " + root.running.length
    if (root.labelMode === "none") return glyph
    return glyph + "  " + root.formatSpeed(root.speed)
  }

  function formatSpeed(bytesPerSecond) {
    var b = Number(bytesPerSecond) || 0
    if (b < 1024) return Math.round(b) + " B/s"
    if (b < 1048576) return (b / 1024).toFixed(0) + " KB/s"
    return (b / 1048576).toFixed(1) + " MB/s"
  }

  function formatBytes(bytes) {
    var b = Number(bytes) || 0
    if (b < 1024) return Math.round(b) + " B"
    if (b < 1048576) return (b / 1024).toFixed(0) + " KB"
    if (b < 1073741824) return (b / 1048576).toFixed(1) + " MB"
    return (b / 1073741824).toFixed(2) + " GB"
  }

  function fraction(task) {
    if (!task.total_bytes || task.total_bytes <= 0) return 0
    return Math.max(0, Math.min(1, task.received_bytes / task.total_bytes))
  }

  function plural(count, one, many) {
    return count === 1 ? one : many
  }

  function statusWord(status) {
    if (status === "running") return "Downloading"
    if (status === "queued") return "Queued"
    if (status === "paused") return "Paused"
    if (status === "completed") return "Done"
    if (status === "failed") return "Failed"
    return String(status)
  }

  function refresh() {
    if (!tasksProc.running) tasksProc.running = true
  }

  function control(id, action) {
    Quickshell.execDetached(["sh", "-c", "PATH=\"$HOME/.local/bin:$PATH\"; exec \"$0\" \"$@\"", cmd, action, "tsk_" + id])
    settle.restart()
  }

  function launchApp() {
    Quickshell.execDetached(["sh", "-c", "command -v omalldownloader >/dev/null 2>&1 && exec omalldownloader"])
  }

  Process {
    id: tasksProc
    command: ["sh", "-c", "PATH=\"$HOME/.local/bin:$PATH\"; exec \"$0\" tasks --json 2>/dev/null", cmd]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var raw = String(text || "").trim()
        if (raw !== "") {
          try {
            var parsed = JSON.parse(raw)
            root.tasks = Array.isArray(parsed) ? parsed : []
          } catch (e) {
            root.tasks = []
          }
        }
        root.loaded = true
      }
    }
    onExited: function (code) {
      if (code !== 0) { root.tasks = []; root.loaded = true }
    }
  }

  // One refresh shortly after an action, so the row shows what happened without waiting
  // for the next tick.
  Timer {
    id: settle
    interval: 500
    repeat: false
    onTriggered: root.refresh()
  }

  Timer {
    interval: root.refreshMs
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: root.refresh()
  }

  ShellIpc {
    target: "dimdoremy.omalldownloader." + root.screenName
    function open(): void { root.open() }
    function close(): void { root.close() }
    function toggle(): void { root.toggle() }
    function refresh(): void { root.refresh() }
    function launch(): void { root.launchApp() }
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.barText
    active: root.busy
    horizontalMargin: 8.75
    tooltipText: root.busy
      ? root.running.length + " downloading · " + root.formatSpeed(root.speed)
      : "OmaLLDownloader"
    onPressed: function (b) {
      if (b === Qt.MiddleButton) root.launchApp()
      else { root.refresh(); root.toggle() }
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(column.implicitHeight)

    readonly property color fg: root.bar ? root.bar.foreground : Commons.Color.foreground
    readonly property string fontName: root.bar ? root.bar.fontFamily : Style.font.family

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onCloseRequested: root.close()

      Column {
        id: column
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: Style.space(10)

        Text {
          text: "Downloads"
          color: panel.fg
          font.family: panel.fontName
          font.pixelSize: Style.font.title
          font.weight: Font.DemiBold
        }

        Text {
          text: root.busy
            ? root.running.length + " " + root.plural(root.running.length, "download", "downloads")
              + " · " + root.formatSpeed(root.speed)
            : (root.tasks.length > 0
                ? root.tasks.length + " " + root.plural(root.tasks.length, "task", "tasks")
                  + ", none running"
                : "Nothing yet")
          color: Commons.Color.muted
          font.family: panel.fontName
          font.pixelSize: Style.font.body
        }

        Rectangle {
          width: parent.width
          height: 1
          color: Commons.Color.muted
          opacity: 0.3
        }

        Text {
          visible: root.loaded && root.tasks.length === 0
          width: parent.width
          text: "No downloads yet. Paste a link in the app, or send one from the browser extension."
          color: Commons.Color.muted
          wrapMode: Text.WordWrap
          font.family: panel.fontName
          font.pixelSize: Style.font.body
        }

        Repeater {
          model: root.tasks.slice(0, 6)

          delegate: Column {
            required property var modelData
            width: column.width
            spacing: Style.space(4)

            Row {
              width: parent.width
              Text {
                width: parent.width - stateText.width - Style.space(8)
                text: modelData.filename || modelData.url
                elide: Text.ElideMiddle
                color: panel.fg
                font.family: panel.fontName
                font.pixelSize: Style.font.body
              }
              Text {
                id: stateText
                text: root.statusWord(modelData.status)
                color: Commons.Color.muted
                font.family: panel.fontName
                font.pixelSize: Style.font.caption
              }
            }

            Rectangle {
              width: parent.width
              height: 3
              color: Commons.Color.muted
              opacity: 0.25

              Rectangle {
                width: parent.width * root.fraction(modelData)
                height: parent.height
                color: Commons.Color.accent
              }
            }

            Row {
              width: parent.width
              Text {
                width: parent.width * 0.6
                text: root.formatBytes(modelData.received_bytes)
                  + (modelData.total_bytes ? " / " + root.formatBytes(modelData.total_bytes) : "")
                  + (modelData.status === "running" && modelData.speed > 0
                      ? "  ·  " + root.formatSpeed(modelData.speed)
                      : "")
                color: Commons.Color.muted
                font.family: panel.fontName
                font.pixelSize: Style.font.caption
              }
              Row {
                width: parent.width * 0.4
                layoutDirection: Qt.RightToLeft
                spacing: Style.space(10)
                Text {
                  text: modelData.status === "completed" || modelData.status === "failed"
                    ? "Remove"
                    : (modelData.status === "running" ? "Pause" : "Resume")
                  color: Commons.Color.accent
                  font.family: panel.fontName
                  font.pixelSize: Style.font.caption
                  MouseArea {
                    anchors.fill: parent
                    anchors.margins: -4
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.control(
                      modelData.id,
                      modelData.status === "completed" || modelData.status === "failed"
                        ? "remove"
                        : (modelData.status === "running" ? "pause" : "resume"))
                  }
                }
              }
            }
          }
        }

        Text {
          text: "Open app"
          color: Commons.Color.accent
          font.family: panel.fontName
          font.pixelSize: Style.font.body
          MouseArea {
            anchors.fill: parent
            anchors.margins: -4
            cursorShape: Qt.PointingHandCursor
            onClicked: { root.launchApp(); root.close() }
          }
        }
      }
    }
  }
}
