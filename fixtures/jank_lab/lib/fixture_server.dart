import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:image/image.dart' as img;

/// A local HTTP and WebSocket server that gives the `network` scenario
/// something to talk to. It binds to the loopback interface only, so nothing
/// leaves the machine.
class FixtureServer {
  FixtureServer._(this._server, this._imageDelay) {
    _sub = _server.listen(_handle);
  }

  /// Starts the server on loopback with an OS-assigned port.
  ///
  /// [imageDelay] is how long `/img/hero_banner.png` waits before answering.
  static Future<FixtureServer> start({
    Duration imageDelay = const Duration(milliseconds: 1400),
  }) async {
    final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
    return FixtureServer._(server, imageDelay);
  }

  final HttpServer _server;
  final Duration _imageDelay;
  late final StreamSubscription<HttpRequest> _sub;
  Uint8List? _png;
  final List<WebSocket> _sockets = [];

  /// `http://127.0.0.1:<port>`.
  Uri get baseUri =>
      Uri(scheme: 'http', host: _server.address.address, port: _server.port);

  Future<void> close() async {
    await _sub.cancel();
    for (final s in _sockets.toList()) {
      await s.close();
    }
    await _server.close(force: true);
  }

  Future<void> _handle(HttpRequest req) async {
    try {
      final path = req.uri.path;
      if (req.method == 'GET' && path == '/ws') {
        if (WebSocketTransformer.isUpgradeRequest(req)) {
          final ws = await WebSocketTransformer.upgrade(req);
          _sockets.add(ws);
          ws.listen(ws.add, onDone: () => _sockets.remove(ws));
          return;
        }
      } else if (req.method == 'GET' && path == '/v2/products') {
        final page = int.tryParse(req.uri.queryParameters['page'] ?? '') ?? 1;
        _json(req, 200, {
          'page': page,
          'items': [
            for (var i = 0; i < 20; i++)
              {
                'id': (page - 1) * 20 + i,
                'name': 'Product ${(page - 1) * 20 + i}',
                'price': 4.99 + i * 1.5,
              },
          ],
        });
        return;
      } else if (req.method == 'POST' && path == '/v2/orders') {
        await req.drain<void>();
        _json(req, 500, {
          'error': 'payment_declined',
          'message': 'Card declined by issuer',
        });
        return;
      } else if (req.method == 'GET' && path == '/img/hero_banner.png') {
        await Future<void>.delayed(_imageDelay);
        final bytes = _png ??= _makePng();
        req.response
          ..statusCode = 200
          ..headers.contentType = ContentType('image', 'png')
          ..contentLength = bytes.length
          ..add(bytes);
        await req.response.close();
        return;
      } else if (req.method == 'GET' && path == '/slow') {
        await Future<void>.delayed(const Duration(seconds: 8));
        _json(req, 200, {'slow': true});
        return;
      } else if (req.method == 'GET' && path == '/files/blob.bin') {
        final bytes = Uint8List(262144);
        for (var i = 0; i < bytes.length; i++) {
          bytes[i] = i & 0xff;
        }
        req.response
          ..statusCode = 200
          ..headers.contentType = ContentType.binary
          ..contentLength = bytes.length
          ..add(bytes);
        await req.response.close();
        return;
      }
      _json(req, 404, {'error': 'not_found'});
    } catch (_) {
      try {
        await req.response.close();
      } catch (_) {}
    }
  }

  void _json(HttpRequest req, int status, Object body) {
    final bytes = utf8.encode(jsonEncode(body));
    req.response
      ..statusCode = status
      ..headers.contentType = ContentType.json
      ..contentLength = bytes.length
      ..add(bytes);
    req.response.close();
  }

  static Uint8List _makePng() {
    final image = img.Image(width: 512, height: 256);
    for (var y = 0; y < 256; y++) {
      for (var x = 0; x < 512; x++) {
        image.setPixelRgb(x, y, x ~/ 2, y, 255 - x ~/ 2);
      }
    }
    return Uint8List.fromList(img.encodePng(image));
  }
}
