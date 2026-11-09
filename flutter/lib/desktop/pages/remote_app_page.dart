import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:uuid/uuid.dart';

import '../../common.dart';
import '../../main.dart';
import '../../models/model.dart';
import '../../models/platform_model.dart';
import '../../utils/multi_window_manager.dart';
import 'package:flutter_hbb/generated_bridge.dart'
    if (dart.library.html) 'package:flutter_hbb/web/bridge.dart';

class _RemoteAppSession {
  _RemoteAppSession(this.peerId, this.onEvent) : sessionId = Uuid().v4obj();

  final String peerId;
  final SessionID sessionId;
  String get appId => sessionId.toString();
  String? frameId;
  String? layoutId;
  final void Function(Map<String, dynamic>) onEvent;
  StreamSubscription<EventToUI>? _subscription;
  bool _attached = false;
  bool _closing = false;
  bool _disconnected = false;
  bool get isClosed => _disconnected;
  Future<void> _sendTail = Future.value();

  void attach() {
    final error = bind.sessionAddExistedSync(
        id: peerId,
        sessionId: sessionId,
        displays: Int32List(0),
        isViewCamera: false);
    if (error.isNotEmpty) {
      onEvent({'kind': 'error', 'message': error});
      return;
    }
    _attached = true;
    _subscription = bind
        .sessionRemoteAppStart(sessionId: sessionId, id: peerId)
        .listen((event) {
      if (_disconnected) return;
      if (event is! EventToUI_Event) return;
      if (event.field0 == 'close') {
        _closed();
        return;
      }
      try {
        final data = jsonDecode(event.field0);
        if (data is Map<String, dynamic> && data['name'] == 'msgbox') {
          final type = data['type']?.toString() ?? '';
          if (type.startsWith('error') || type == 'connection_error') {
            _closed(data['text']?.toString());
          }
          return;
        }
        if (data is Map<String, dynamic> &&
            (data['name'] == 'remote_app_ready' ||
                (data['name'] == 'remote_app' && data['app_id'] == appId))) {
          if (data['kind'] == 'closed') _disconnected = true;
          onEvent(data);
        }
      } catch (e) {
        debugPrint('Remote application event: $e');
      }
    }, onError: (Object e) {
      _closed(e.toString());
    });
  }

  void _closed([String? message]) {
    _disconnected = true;
    onEvent({
      'kind': 'closed',
      'message': message == null || message.isEmpty
          ? translate('Connection closed')
          : translate(message),
    });
  }

  Future<void> request(String command, [Map<String, dynamic> args = const {}]) {
    if (_disconnected || (_closing && command != 'stop')) return Future.value();
    final request = jsonEncode({
      'app_id': appId,
      'command': command,
      if (frameId != null) 'frame_id': frameId,
      if (layoutId != null) 'layout_id': layoutId,
      ...args,
    });
    _sendTail = _sendTail.then((_) async {
      if (!_attached || _disconnected) return;
      try {
        await bind.sessionRemoteAppRequest(
            sessionId: sessionId, request: request);
      } catch (e) {
        onEvent({'kind': 'error', 'message': e.toString()});
      }
    });
    return _sendTail;
  }

  Future<void> close({bool stop = false}) async {
    if (!_attached || _closing) return;
    _closing = true;
    if (stop) {
      await request('stop');
    } else {
      await _sendTail;
    }
    _attached = false;
    await bind.sessionRemoteAppClose(sessionId: sessionId);
    await _subscription?.cancel();
  }
}

void showRemoteApps(BuildContext context, FFI ffi) {
  showDialog<void>(
      context: context, builder: (_) => _RemoteAppChooser(peerId: ffi.id));
}

class _RemoteAppChooser extends StatefulWidget {
  const _RemoteAppChooser({required this.peerId});
  final String peerId;

  @override
  State<_RemoteAppChooser> createState() => _RemoteAppChooserState();
}

class _RemoteAppChooserState extends State<_RemoteAppChooser> {
  late final _RemoteAppSession _session;
  List<dynamic> _windows = [];
  String? _error;
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _session = _RemoteAppSession(widget.peerId, _onEvent)..attach();
  }

  void _onEvent(Map<String, dynamic> event) {
    if (!mounted) return;
    if (event['name'] == 'remote_app_ready') {
      _refresh();
    } else if (event['kind'] == 'windows') {
      try {
        final windows = jsonDecode(event['windows'] as String);
        setState(() {
          _windows = windows as List<dynamic>;
          _error = null;
          _loading = false;
        });
      } catch (e) {
        setState(() {
          _error = e.toString();
          _loading = false;
        });
      }
    } else if (event['kind'] == 'error' || event['kind'] == 'closed') {
      setState(() {
        _error = event['message']?.toString() ??
            translate('Remote application error');
        _loading = false;
      });
    }
  }

  void _refresh() {
    setState(() {
      _loading = true;
      _error = null;
    });
    unawaited(_session.request('list'));
  }

  Future<void> _open(Map<String, dynamic> app) async {
    try {
      final title = app['title']?.toString() ?? '';
      final controller = await DesktopMultiWindow.createWindow(jsonEncode({
        'type': WindowType.RemoteDesktop.index,
        'remote_app': true,
        'id': widget.peerId,
        'window_id': app['id'].toString(),
        'title': title,
      }));
      await controller.setFrame(const Offset(0, 0) & const Size(1000, 700));
      await controller.center();
      await controller.setTitle('$title - ${widget.peerId}');
      await controller.show();
    } catch (e) {
      if (mounted) setState(() => _error = e.toString());
    }
  }

  @override
  void dispose() {
    unawaited(_session.close());
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => AlertDialog(
        title: Text(translate('Remote applications')),
        content: SizedBox(
          width: 480,
          height: 360,
          child: _loading
              ? const Center(child: CircularProgressIndicator())
              : _error != null
                  ? Center(child: SelectableText(_error!))
                  : _windows.isEmpty
                      ? Center(child: Text(translate('No applications found')))
                      : ListView.builder(
                          itemCount: _windows.length,
                          itemBuilder: (context, index) {
                            final app = _windows[index] as Map<String, dynamic>;
                            return ListTile(
                              title: Text(app['title']?.toString() ?? ''),
                              onTap: () => _open(app),
                            );
                          },
                        ),
        ),
        actions: [
          TextButton(
              onPressed: _session.isClosed ? null : _refresh,
              child: Text(translate('Refresh'))),
          TextButton(
              onPressed: () => Navigator.pop(context),
              child: Text(translate('Close'))),
        ],
      );
}

class RemoteAppPage extends StatefulWidget {
  const RemoteAppPage({super.key, required this.params});
  final Map<String, dynamic> params;

  @override
  State<RemoteAppPage> createState() => _RemoteAppPageState();
}

class _RemoteAppPageState extends State<RemoteAppPage>
    with MultiWindowListener {
  late final _RemoteAppSession _session;
  late final FocusNode _textFocus;
  final _textController = TextEditingController();
  final Set<int> _forwardedKeys = {};
  ui.Image? _image;
  Size _available = Size.zero;
  String? _error;
  late String _title;
  bool _closing = false;
  bool _failed = false;
  int _buttons = 0;

  @override
  void initState() {
    super.initState();
    DesktopMultiWindow.setMethodHandler((_, __) async {});
    DesktopMultiWindow.addListener(this);
    _title = widget.params['title']?.toString() ?? '';
    _textFocus = FocusNode(onKeyEvent: _key);
    _textController.addListener(_commitText);
    _session = _RemoteAppSession(widget.params['id'] as String, _onEvent)
      ..attach();
  }

  void _commitText() {
    final value = _textController.value;
    if (value.text.isEmpty ||
        (value.composing.isValid && !value.composing.isCollapsed)) return;
    if (!_failed) {
      unawaited(_session.request('key', {'text': value.text, 'down': true}));
    }
    _textController.clear();
  }

  void _onEvent(Map<String, dynamic> event) {
    if (!mounted || _closing || _failed) return;
    if (event['name'] == 'remote_app_ready') {
      unawaited(
          _session.request('start', {'window_id': widget.params['window_id']}));
    } else if (event['kind'] == 'frame') {
      unawaited(_frame(event));
    } else if (event['kind'] == 'error' || event['kind'] == 'closed') {
      setState(() {
        if (event['kind'] == 'closed') {
          _failed = true;
          _forwardedKeys.clear();
          _buttons = 0;
        }
        _error = event['message']?.toString() ??
            _error ??
            translate('Application closed');
      });
    }
  }

  Future<void> _frame(Map<String, dynamic> event) async {
    var decoded = false;
    try {
      final codec =
          await ui.instantiateImageCodec(base64Decode(event['jpeg'] as String));
      final frame = await codec.getNextFrame();
      codec.dispose();
      if (!mounted || _closing || _failed) {
        frame.image.dispose();
        return;
      }
      final old = _image;
      final title = event['title']?.toString() ?? '';
      if (title.isNotEmpty && title != _title) {
        _title = title;
        unawaited(WindowController.fromWindowId(kWindowId!)
            .setTitle('$title - ${widget.params['id']}')
            .catchError(
                (Object e) => debugPrint('Remote application title: $e')));
      }
      setState(() {
        _image = frame.image;
        _session.frameId = event['frame_id']?.toString();
        _session.layoutId = event['layout_id']?.toString();
        _error = null;
      });
      await WidgetsBinding.instance.endOfFrame;
      old?.dispose();
      decoded = true;
    } catch (e) {
      if (mounted) setState(() => _error = e.toString());
    } finally {
      if (decoded && !_closing && !_failed) await _session.request('ack');
    }
  }

  Offset? _remotePoint(Offset position, {bool clamp = false}) {
    final image = _image;
    if (image == null || _available.isEmpty) return null;
    final scale = (_available.width / image.width)
        .clamp(0.0, _available.height / image.height);
    if (scale <= 0) return null;
    final size = Size(image.width * scale, image.height * scale);
    final origin = Offset((_available.width - size.width) / 2,
        (_available.height - size.height) / 2);
    final local = position - origin;
    if (!clamp && !(Offset.zero & size).contains(local)) return null;
    return Offset((local.dx / scale).clamp(0, image.width - 1).toDouble(),
        (local.dy / scale).clamp(0, image.height - 1).toDouble());
  }

  void _pointer(int kind, PointerEvent event, {int button = 0}) {
    if (_failed) return;
    final point =
        _remotePoint(event.localPosition, clamp: _buttons != 0 || kind == 2);
    if (point == null) return;
    unawaited(_session.request('pointer', {
      'kind': kind,
      'button': button,
      'x': point.dx.round(),
      'y': point.dy.round(),
      if (event is PointerScrollEvent) ...{
        'scroll_x': event.scrollDelta.dx == 0
            ? 0
            : event.scrollDelta.dx.sign.toInt() * -120,
        'scroll_y': event.scrollDelta.dy == 0
            ? 0
            : event.scrollDelta.dy.sign.toInt() * -120,
      },
    }));
  }

  int _button(int mask) {
    if ((mask & kPrimaryMouseButton) != 0) return 1;
    if ((mask & kSecondaryMouseButton) != 0) return 2;
    if ((mask & kMiddleMouseButton) != 0) return 3;
    if ((mask & kBackMouseButton) != 0) return 4;
    if ((mask & kForwardMouseButton) != 0) return 5;
    return 0;
  }

  KeyEventResult _key(FocusNode node, KeyEvent event) {
    if (_failed) return KeyEventResult.handled;
    final vk = _virtualKey(event.logicalKey);
    if (vk == null) return KeyEventResult.ignored;
    final down = event is! KeyUpEvent;
    final keyboard = HardwareKeyboard.instance;
    final character = event.character;
    final printable = character != null &&
        character.isNotEmpty &&
        character.runes.first >= 0x20 &&
        character.runes.first != 0x7f;
    if (down &&
        printable &&
        !keyboard.isControlPressed &&
        !keyboard.isAltPressed &&
        !keyboard.isMetaPressed) {
      return KeyEventResult.ignored;
    }
    if (!down && !_forwardedKeys.contains(vk)) return KeyEventResult.ignored;
    if (down) {
      _forwardedKeys.add(vk);
    } else {
      _forwardedKeys.remove(vk);
    }
    unawaited(_session.request('key', {'virtual_key': vk, 'down': down}));
    return KeyEventResult.handled;
  }

  int? _virtualKey(LogicalKeyboardKey key) {
    final id = key.keyId;
    if (id >= 0x61 && id <= 0x7a) return id - 0x20;
    if (id >= 0x30 && id <= 0x39) return id;
    final keys = <LogicalKeyboardKey, int>{
      LogicalKeyboardKey.backspace: 0x08,
      LogicalKeyboardKey.tab: 0x09,
      LogicalKeyboardKey.enter: 0x0d,
      LogicalKeyboardKey.shiftLeft: 0xa0,
      LogicalKeyboardKey.shiftRight: 0xa1,
      LogicalKeyboardKey.controlLeft: 0xa2,
      LogicalKeyboardKey.controlRight: 0xa3,
      LogicalKeyboardKey.altLeft: 0xa4,
      LogicalKeyboardKey.altRight: 0xa5,
      LogicalKeyboardKey.metaLeft: 0x5b,
      LogicalKeyboardKey.metaRight: 0x5c,
      LogicalKeyboardKey.escape: 0x1b,
      LogicalKeyboardKey.space: 0x20,
      LogicalKeyboardKey.pageUp: 0x21,
      LogicalKeyboardKey.pageDown: 0x22,
      LogicalKeyboardKey.end: 0x23,
      LogicalKeyboardKey.home: 0x24,
      LogicalKeyboardKey.arrowLeft: 0x25,
      LogicalKeyboardKey.arrowUp: 0x26,
      LogicalKeyboardKey.arrowRight: 0x27,
      LogicalKeyboardKey.arrowDown: 0x28,
      LogicalKeyboardKey.insert: 0x2d,
      LogicalKeyboardKey.delete: 0x2e,
      LogicalKeyboardKey.capsLock: 0x14,
      LogicalKeyboardKey.f1: 0x70,
      LogicalKeyboardKey.f2: 0x71,
      LogicalKeyboardKey.f3: 0x72,
      LogicalKeyboardKey.f4: 0x73,
      LogicalKeyboardKey.f5: 0x74,
      LogicalKeyboardKey.f6: 0x75,
      LogicalKeyboardKey.f7: 0x76,
      LogicalKeyboardKey.f8: 0x77,
      LogicalKeyboardKey.f9: 0x78,
      LogicalKeyboardKey.f10: 0x79,
      LogicalKeyboardKey.f11: 0x7a,
      LogicalKeyboardKey.f12: 0x7b,
    };
    return keys[key];
  }

  @override
  void onWindowBlur() {
    _releaseInput();
  }

  void _releaseInput() {
    _forwardedKeys.clear();
    _buttons = 0;
    if (!_failed) unawaited(_session.request('release'));
  }

  @override
  void onWindowClose() async {
    if (_closing) return;
    _closing = true;
    await _session.close(stop: true);
    final controller = WindowController.fromWindowId(kWindowId!);
    await controller.setPreventClose(false);
    await controller.close();
  }

  @override
  void dispose() {
    DesktopMultiWindow.removeListener(this);
    DesktopMultiWindow.setMethodHandler(null);
    _textFocus.dispose();
    _textController.dispose();
    _image?.dispose();
    if (!_closing) unawaited(_session.close(stop: true));
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        body: Column(children: [
          Row(children: [
            TextButton.icon(
              onPressed: _failed
                  ? null
                  : () {
                      if (!_available.isEmpty) {
                        unawaited(_session.request('resize', {
                          'width': _available.width.round(),
                          'height': _available.height.round(),
                        }));
                      }
                    },
              icon: const Icon(Icons.fit_screen),
              label: Text(translate('Fit to window')),
            ),
            if (_error != null) Expanded(child: Text(_error!)),
          ]),
          Expanded(child: LayoutBuilder(builder: (context, constraints) {
            _available = constraints.biggest;
            return Focus(
              onFocusChange: (focused) {
                if (!focused) _releaseInput();
              },
              child: Stack(children: [
                Positioned.fill(
                    child: Listener(
                  onPointerHover: (e) => _pointer(0, e),
                  onPointerMove: (e) => _pointer(0, e),
                  onPointerDown: (e) {
                    if (_failed) return;
                    if (_remotePoint(e.localPosition) == null) return;
                    _textFocus.requestFocus();
                    final changed = e.buttons & ~_buttons;
                    _buttons = e.buttons;
                    _pointer(1, e, button: _button(changed));
                  },
                  onPointerUp: (e) {
                    if (_failed) return;
                    final changed = _buttons & ~e.buttons;
                    _pointer(2, e, button: _button(changed));
                    _buttons = e.buttons;
                  },
                  onPointerCancel: (_) => _releaseInput(),
                  onPointerSignal: (e) {
                    if (e is PointerScrollEvent) _pointer(3, e);
                  },
                  child: ColoredBox(
                    color: Colors.black,
                    child: _image == null
                        ? const Center(child: CircularProgressIndicator())
                        : SizedBox.expand(
                            child:
                                RawImage(image: _image, fit: BoxFit.contain)),
                  ),
                )),
                Positioned(
                  left: 0,
                  top: 0,
                  width: 1,
                  height: 1,
                  child: IgnorePointer(
                    child: Opacity(
                      opacity: 0,
                      child: TextField(
                        focusNode: _textFocus,
                        controller: _textController,
                        decoration:
                            const InputDecoration.collapsed(hintText: ''),
                        style: const TextStyle(fontSize: 1, height: 1),
                      ),
                    ),
                  ),
                ),
              ]),
            );
          })),
        ]),
      );
}
