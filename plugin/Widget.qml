import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import qs.Ui

// Bar widget for Meeting Recorder. While a meeting is being recorded it
// shows a pulsing dot, a live waveform (mic above the line, computer audio below
// it, fainter) and the elapsed time. While the meeting is transcribed afterwards
// it shows a steady dot and the progress. Hidden otherwise. Clicking it brings
// the recorder window back. All data comes from `omarchy-meeting-recorder watch`
// as NDJSON.
//
// The waveform repaints on every state line (20 a second while recording) and
// the dot pulses every frame, so the bar redraws continuously on every screen.
// With the `liveWaveform` setting off the widget shows a steady dot and the time
// and redraws once a second, for those who keep the recorder window in view.
BarWidget {
  id: root
  moduleName: "jankeesvw.meeting-recorder"

  readonly property int historyLength: 28
  readonly property int maxTitleLength: 200

  property string recorderState: "off"
  property int elapsed: 0
  property real progress: 0
  property string title: ""
  property var micHistory: emptyHistory()
  property var computerHistory: emptyHistory()

  readonly property bool shown: recorderState === "recording" || recorderState === "paused"
                                || recorderState === "stopping"
                                || recorderState === "transcribing"
  readonly property color foreground: bar ? bar.barForeground : Commons.Color.foreground
  readonly property color recordColor: Commons.Color.urgent
  readonly property bool liveWaveform: setting("liveWaveform", true) !== false

  visible: shown
  implicitWidth: shown ? content.implicitWidth + Style.space(14) : 0
  implicitHeight: barSize

  function emptyHistory() {
    var list = []
    for (var i = 0; i < historyLength; i++) list.push(0)
    return list
  }

  function level(value) {
    var n = Number(value)
    return isFinite(n) ? Math.max(0, Math.min(1, n)) : 0
  }

  function pushLevel(history, value) {
    var next = history.slice(1)
    next.push(level(value))
    return next
  }

  function apply(line) {
    if (line.length > 4096) return
    var data
    try { data = JSON.parse(line) } catch (e) { return }
    if (!data || typeof data.state !== "string") return

    var wasShown = root.shown
    root.recorderState = /^(off|idle|recording|paused|stopping|transcribing|done)$/.test(data.state) ? data.state : "off"
    root.elapsed = Math.max(0, Math.floor(Number(data.elapsed) || 0))
    root.progress = root.level(data.progress)
    root.title = typeof data.title === "string" ? data.title.slice(0, root.maxTitleLength) : ""

    if (root.recorderState === "recording" && root.liveWaveform) {
      root.micHistory = pushLevel(root.micHistory, data.mic)
      root.computerHistory = pushLevel(root.computerHistory, data.computer)
      wave.requestPaint()
    } else if (!root.shown && wasShown) {
      root.micHistory = emptyHistory()
      root.computerHistory = emptyHistory()
    }
  }

  function clock(secs) {
    var h = Math.floor(secs / 3600), m = Math.floor(secs / 60) % 60, s = secs % 60
    var mm = (m < 10 ? "0" : "") + m, ss = (s < 10 ? "0" : "") + s
    return h > 0 ? h + ":" + mm + ":" + ss : mm + ":" + ss
  }

  Process {
    id: watcher
    running: true
    command: ["omarchy-meeting-recorder", "watch"]
    stdout: SplitParser {
      onRead: function(line) { root.apply(line) }
    }
  }

  // The watcher only ends if the binary is missing or crashed; try again later.
  Timer {
    interval: 5000
    running: !watcher.running
    repeat: true
    onTriggered: watcher.running = true
  }

  Row {
    id: content
    anchors.centerIn: parent
    spacing: Style.space(6)

    Rectangle {
      id: dot
      anchors.verticalCenter: parent.verticalCenter
      width: Style.space(7)
      height: width
      radius: width / 2
      // Dimmed and still while paused.
      color: root.recorderState === "paused"
             ? Qt.rgba(root.recordColor.r, root.recordColor.g, root.recordColor.b, 0.4)
             : root.recordColor
      opacity: 1

      SequentialAnimation on opacity {
        running: root.recorderState === "recording" && root.liveWaveform
        loops: Animation.Infinite
        alwaysRunToEnd: true
        NumberAnimation { to: 0.3; duration: 700; easing.type: Easing.InOutSine }
        NumberAnimation { to: 1.0; duration: 700; easing.type: Easing.InOutSine }
      }
    }

    Canvas {
      id: wave
      anchors.verticalCenter: parent.verticalCenter
      width: Style.space(root.historyLength * 2)
      height: Math.round(root.barSize * 0.62)
      visible: !root.vertical && root.recorderState === "recording" && root.liveWaveform

      onPaint: {
        var ctx = getContext("2d")
        ctx.reset()
        var mid = height / 2
        var step = width / root.historyLength
        var bar = Math.max(1, step * 0.55)
        var fg = root.foreground

        ctx.fillStyle = Qt.rgba(fg.r, fg.g, fg.b, 0.18)
        ctx.fillRect(0, mid - 0.5, width, 1)

        for (var i = 0; i < root.historyLength; i++) {
          var alpha = 0.4 + 0.6 * (i / root.historyLength)
          var up = Math.max(0.5, root.micHistory[i] * (mid - 1))
          var down = Math.max(0.5, root.computerHistory[i] * (mid - 1))
          ctx.fillStyle = Qt.rgba(fg.r, fg.g, fg.b, alpha)
          ctx.fillRect(i * step, mid - up, bar, up)
          ctx.fillStyle = Qt.rgba(fg.r, fg.g, fg.b, alpha * 0.55)
          ctx.fillRect(i * step, mid, bar, down)
        }
      }
    }

    Text {
      anchors.verticalCenter: parent.verticalCenter
      textFormat: Text.PlainText
      text: root.recorderState === "stopping" ? "saving…"
          : root.recorderState === "paused" ? "paused " + root.clock(root.elapsed)
          : root.recorderState === "transcribing" ? "transcribing " + Math.round(root.progress * 100) + "%"
          : root.clock(root.elapsed)
      color: root.foreground
      font.family: bar ? bar.fontFamily : Style.font.family
      font.pixelSize: Style.font.body
      font.features: { "tnum": 1 }
    }
  }

  MouseArea {
    anchors.fill: parent
    enabled: root.shown
    hoverEnabled: true
    cursorShape: Qt.PointingHandCursor
    onClicked: if (root.bar) root.bar.run("omarchy-meeting-recorder")
    onEntered: if (root.bar) root.bar.showTooltip(root, root.title ? "Recording: " + root.title : "Recording a meeting")
    onExited: if (root.bar) root.bar.hideTooltip(root)
  }
}
