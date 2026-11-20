import 'dart:ui' as ui;

import 'package:flutter/material.dart';

import '../ext.dart';

/// Provokes frames over budget on both threads. `HeavyBuildPanel` busy-waits
/// in `build` while an animation keeps frames coming (UI-thread jank), and the
/// "Raster heavy" switch stacks blurred, translucent layers (raster jank).
/// Expect it in the fdt Performance tool's frame chart and the CPU profiler.
class JankScreen extends StatefulWidget {
  const JankScreen({super.key});

  @override
  State<JankScreen> createState() => _JankScreenState();
}

class _JankScreenState extends State<JankScreen>
    with SingleTickerProviderStateMixin {
  late final AnimationController _controller;
  bool _raster = false;

  @override
  void initState() {
    super.initState();
    // Created here, not lazily: first touching a `late final` ticker inside
    // `dispose` looks up the (deactivated) ancestor chain, which throws in
    // debug on Flutter 3.41-3.44.
    _controller = AnimationController(
      vsync: this,
      duration: const Duration(seconds: 2),
    );
    _unregister = registerJankActions({
      'raster_on': () => _setRaster(true),
      'raster_off': () => _setRaster(false),
    });
  }

  late final VoidCallback _unregister;

  void _setRaster(bool value) {
    setState(() => _raster = value);
    if (value) {
      _controller.repeat();
    } else {
      _controller.stop();
    }
  }

  @override
  void dispose() {
    _unregister();
    _controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Jank')),
      body: Column(
        children: [
          const HeavyBuildPanel(),
          SwitchListTile(
            title: const Text('Raster heavy'),
            value: _raster,
            onChanged: _setRaster,
          ),
          if (_raster)
            Expanded(child: _RasterHeavy(animation: _controller))
          else
            const Spacer(),
        ],
      ),
    );
  }
}

/// A panel whose `build` burns [buildCost] of CPU on every animation frame.
class HeavyBuildPanel extends StatefulWidget {
  const HeavyBuildPanel({
    super.key,
    this.buildCost = const Duration(milliseconds: 24),
  });

  final Duration buildCost;

  @override
  State<HeavyBuildPanel> createState() => _HeavyBuildPanelState();
}

class _HeavyBuildPanelState extends State<HeavyBuildPanel>
    with SingleTickerProviderStateMixin {
  late final AnimationController _controller = AnimationController(
    vsync: this,
    duration: const Duration(seconds: 1),
  )..repeat();

  @override
  void initState() {
    super.initState();
    _controller.addListener(() => setState(() {}));
  }

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final watch = Stopwatch()..start();
    while (watch.elapsed < widget.buildCost) {}
    return Padding(
      padding: const EdgeInsets.all(16),
      child: LinearProgressIndicator(value: _controller.value),
    );
  }
}

class _RasterHeavy extends StatelessWidget {
  const _RasterHeavy({required this.animation});

  final Animation<double> animation;

  @override
  Widget build(BuildContext context) {
    return AnimatedBuilder(
      animation: animation,
      builder: (context, _) {
        return Stack(
          children: [
            for (var i = 0; i < 160; i++)
              Positioned(
                left: 8.0 + (i % 8) * 40 + animation.value * 20,
                top: 8.0 + (i ~/ 8) * 24,
                width: 160,
                height: 120,
                child: Opacity(
                  opacity: 0.5,
                  child: BackdropFilter(
                    filter: ui.ImageFilter.blur(sigmaX: 12, sigmaY: 12),
                    child: ColoredBox(
                      color: Colors.primaries[i % Colors.primaries.length],
                    ),
                  ),
                ),
              ),
          ],
        );
      },
    );
  }
}
