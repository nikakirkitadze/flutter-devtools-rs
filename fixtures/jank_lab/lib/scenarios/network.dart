import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;

import '../fixture_server.dart';

/// Provokes network traffic against a loopback [FixtureServer]: a JSON GET, a
/// failing POST (500), a slow image, a 256 KiB download, a burst of 100
/// requests and a WebSocket that sends a ping every 2 s. Expect all of it in
/// the fdt Network tool.
class NetworkScreen extends StatefulWidget {
  const NetworkScreen({super.key});

  @override
  State<NetworkScreen> createState() => _NetworkScreenState();
}

class _NetworkScreenState extends State<NetworkScreen> {
  FixtureServer? _server;
  final List<String> _lines = [];
  bool _showImage = false;
  WebSocket? _ws;
  Timer? _wsTimer;
  int _pings = 0;

  @override
  void initState() {
    super.initState();
    FixtureServer.start().then((s) {
      if (!mounted) {
        s.close();
        return;
      }
      setState(() => _server = s);
    });
  }

  @override
  void dispose() {
    _wsTimer?.cancel();
    _ws?.close();
    _server?.close();
    super.dispose();
  }

  void _log(String line) {
    if (mounted) setState(() => _lines.insert(0, line));
  }

  Future<void> _run(
    String label,
    Future<String> Function(Uri base) body,
  ) async {
    final base = _server?.baseUri;
    if (base == null) return;
    try {
      _log('$label: ${await body(base)}');
    } catch (e) {
      _log('$label: error $e');
    }
  }

  Future<void> _loadProducts() => _run('GET products', (base) async {
    final r = await http.get(base.resolve('/v2/products?page=3'));
    return '${r.statusCode}, ${r.bodyBytes.length} bytes';
  });

  Future<void> _placeOrder() => _run('POST order', (base) async {
    final r = await http.post(base.resolve('/v2/orders'), body: '{"cart":1}');
    return '${r.statusCode}';
  });

  Future<void> _download() => _run('GET blob', (base) async {
    final r = await http.get(base.resolve('/files/blob.bin'));
    return '${r.statusCode}, ${r.bodyBytes.length} bytes';
  });

  Future<void> _burst() => _run('Burst', (base) async {
    final results = await Future.wait([
      for (var i = 0; i < 100; i++)
        http.get(base.resolve('/v2/products?page=${i % 5 + 1}')),
    ]);
    return '${results.length} responses';
  });

  Future<void> _openSocket() async {
    final base = _server?.baseUri;
    if (base == null || _ws != null) return;
    final ws = await WebSocket.connect('ws://127.0.0.1:${base.port}/ws');
    _ws = ws;
    ws.listen((m) => _log('ws echo: $m'), onDone: () => _ws = null);
    _wsTimer = Timer.periodic(const Duration(seconds: 2), (_) {
      ws.add('ping ${++_pings}');
    });
    _log('ws: connected');
  }

  @override
  Widget build(BuildContext context) {
    final base = _server?.baseUri;
    return Scaffold(
      appBar: AppBar(title: const Text('Network')),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          Text(base == null ? 'Starting server...' : 'Server at $base'),
          Wrap(
            spacing: 8,
            children: [
              FilledButton(
                onPressed: _loadProducts,
                child: const Text('Load products'),
              ),
              FilledButton(
                onPressed: _placeOrder,
                child: const Text('Place order'),
              ),
              FilledButton(
                onPressed: () => setState(() => _showImage = true),
                child: const Text('Load hero image'),
              ),
              FilledButton(
                onPressed: _download,
                child: const Text('Download blob'),
              ),
              FilledButton(
                onPressed: _burst,
                child: const Text('Burst 100 requests'),
              ),
              FilledButton(
                onPressed: _openSocket,
                child: const Text('Open WebSocket'),
              ),
            ],
          ),
          if (_showImage && base != null)
            Image.network(
              base.resolve('/img/hero_banner.png').toString(),
              errorBuilder: (_, e, _) => Text('Image failed: $e'),
            ),
          for (final l in _lines) Text(l),
        ],
      ),
    );
  }
}
