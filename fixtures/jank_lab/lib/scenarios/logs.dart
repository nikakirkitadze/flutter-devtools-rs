import 'dart:developer' as developer;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../ext.dart';

/// Provokes noisy logging: leveled `developer.log` records from two logger
/// names, a burst of 1,000 `print` lines, a 30-line record, a reported
/// Flutter error, and a widget that throws while building. Expect it in the
/// fdt Logging tool.
class LogsScreen extends StatefulWidget {
  const LogsScreen({super.key});

  @override
  State<LogsScreen> createState() => _LogsScreenState();
}

class _LogsScreenState extends State<LogsScreen> {
  bool _throw = false;
  late final VoidCallback _unregister;

  @override
  void initState() {
    super.initState();
    _unregister = registerJankActions({
      'emit_levels': _emitLevels,
      'print_burst': _printBurst,
      'multi_line': _multiLine,
      'report_error': _reportError,
      'throw_in_build': () {
        if (mounted) setState(() => _throw = true);
      },
    });
  }

  @override
  void dispose() {
    _unregister();
    super.dispose();
  }

  void _emitLevels() {
    developer.log('fine detail', name: 'jank_lab.net', level: 500);
    developer.log('request started', name: 'jank_lab.ui', level: 800);
    developer.log('slow response', name: 'jank_lab.net', level: 900);
    developer.log('request failed', name: 'jank_lab.ui', level: 1000);
  }

  void _printBurst() {
    for (var i = 0; i < 1000; i++) {
      // ignore: avoid_print
      print('burst line $i');
    }
  }

  void _multiLine() {
    developer.log(
      [for (var i = 1; i <= 30; i++) 'line $i of 30'].join('\n'),
      name: 'jank_lab.ui',
    );
  }

  void _reportError() {
    FlutterError.reportError(
      FlutterErrorDetails(
        exception: StateError('Reported by jank_lab'),
        library: 'jank_lab',
        context: ErrorDescription('while reporting a test error'),
        informationCollector: () => [
          ErrorSummary('Something looked wrong'),
          ErrorDescription('The widget tree was in an odd state.'),
          DiagnosticsProperty<String>('screen', 'LogsScreen'),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Logs')),
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          FilledButton(
            onPressed: _emitLevels,
            child: const Text('Emit levels'),
          ),
          FilledButton(
            onPressed: _printBurst,
            child: const Text('Print burst'),
          ),
          FilledButton(
            onPressed: _multiLine,
            child: const Text('Multi-line record'),
          ),
          FilledButton(
            onPressed: _reportError,
            child: const Text('Report Flutter error'),
          ),
          FilledButton(
            onPressed: () => setState(() => _throw = true),
            child: const Text('Throw in build'),
          ),
          if (_throw) const _Thrower(),
        ],
      ),
    );
  }
}

class _Thrower extends StatelessWidget {
  const _Thrower();

  @override
  Widget build(BuildContext context) {
    throw StateError('Deliberate failure in build');
  }
}
