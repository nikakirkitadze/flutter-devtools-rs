import 'package:flutter/material.dart';

/// Provokes a very deep and very wide widget tree: [depth] nested `Padding`
/// widgets around a `Wrap` of many tiny leaves. Expect it to stress the fdt
/// Inspector's tree loading and virtualization.
class DeepTreeScreen extends StatelessWidget {
  const DeepTreeScreen({super.key, this.depth = 200, this.nodes = 50000});

  final int depth;
  final int nodes;

  @override
  Widget build(BuildContext context) {
    final leafCount = (nodes - depth) ~/ 2;
    Widget child = Wrap(
      children: [
        for (var i = 0; i < leafCount; i++)
          SizedBox(
            width: 2,
            height: 2,
            child: ColoredBox(
              color: Colors.primaries[i % Colors.primaries.length],
            ),
          ),
      ],
    );
    for (var i = 0; i < depth; i++) {
      child = Padding(padding: EdgeInsets.zero, child: child);
    }
    return Scaffold(
      appBar: AppBar(title: const Text('Deep tree')),
      body: SingleChildScrollView(child: child),
    );
  }
}
