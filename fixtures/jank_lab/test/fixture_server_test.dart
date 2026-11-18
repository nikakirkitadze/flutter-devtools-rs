import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:jank_lab/fixture_server.dart';

void main() {
  late FixtureServer server;

  setUp(() async {
    server = await FixtureServer.start(imageDelay: Duration.zero);
  });

  tearDown(() async {
    await server.close();
  });

  test('products returns a page of 20 items', () async {
    final res = await http.get(server.baseUri.resolve('/v2/products?page=3'));
    expect(res.statusCode, 200);
    expect(res.headers['content-type'], contains('application/json'));
    final body = jsonDecode(res.body) as Map<String, dynamic>;
    expect(body['page'], 3);
    expect((body['items'] as List).length, 20);
  });

  test('orders fails with 500 payment_declined', () async {
    final res = await http.post(server.baseUri.resolve('/v2/orders'));
    expect(res.statusCode, 500);
    final body = jsonDecode(res.body) as Map<String, dynamic>;
    expect(body['error'], 'payment_declined');
  });

  test('hero image is a PNG', () async {
    final res = await http.get(server.baseUri.resolve('/img/hero_banner.png'));
    expect(res.statusCode, 200);
    expect(res.headers['content-type'], 'image/png');
    expect(res.bodyBytes.sublist(0, 4), [137, 80, 78, 71]);
  });

  test('blob is 262144 bytes', () async {
    final res = await http.get(server.baseUri.resolve('/files/blob.bin'));
    expect(res.statusCode, 200);
    expect(res.bodyBytes.length, 262144);
  });

  test('websocket echoes messages', () async {
    final ws = await WebSocket.connect(
      'ws://127.0.0.1:${server.baseUri.port}/ws',
    );
    ws.add('ping 1');
    expect(await ws.first, 'ping 1');
    await ws.close();
  });

  test('unknown route is 404', () async {
    final res = await http.get(server.baseUri.resolve('/nope'));
    expect(res.statusCode, 404);
  });
}
