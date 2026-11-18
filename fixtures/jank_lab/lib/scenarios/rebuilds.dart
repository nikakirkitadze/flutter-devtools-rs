import 'dart:async';

import 'package:flutter/material.dart';

/// Provokes a rebuild hotspot. `CartScreen` calls `setState` every 16 ms and
/// its `build` returns the badge together with the whole product grid, so 64
/// tiles rebuild on every tick. Expect it in the fdt Performance tool's
/// rebuild counts and the widget inspector.
class CartScreen extends StatefulWidget {
  const CartScreen({super.key});

  @override
  State<CartScreen> createState() => _CartScreenState();
}

class _CartScreenState extends State<CartScreen> {
  int _count = 0;
  Timer? _timer;

  @override
  void initState() {
    super.initState();
    _timer = Timer.periodic(const Duration(milliseconds: 16), (_) {
      setState(() => _count++);
    });
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Rebuilds')),
      body: Column(
        children: [
          CartBadge(count: _count),
          const Expanded(child: ProductGrid()),
        ],
      ),
    );
  }
}

/// Shows the tick counter.
class CartBadge extends StatelessWidget {
  const CartBadge({super.key, required this.count});

  final int count;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.all(8),
      child: Badge(
        label: Text('$count'),
        child: const Icon(Icons.shopping_cart),
      ),
    );
  }
}

/// A grid of [tileCount] tiles.
class ProductGrid extends StatelessWidget {
  const ProductGrid({super.key});

  static const tileCount = 64;

  @override
  Widget build(BuildContext context) {
    // A Wrap inside a scroll view builds every tile, on screen or not.
    return SingleChildScrollView(
      child: Wrap(
        children: [
          for (var i = 0; i < tileCount; i++)
            SizedBox(width: 96, height: 96, child: ProductTile(index: i)),
        ],
      ),
    );
  }
}

/// One product card.
class ProductTile extends StatelessWidget {
  const ProductTile({super.key, required this.index});

  final int index;

  @override
  Widget build(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(4),
        child: Column(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            const Icon(Icons.inventory_2),
            Text('Item $index', overflow: TextOverflow.ellipsis),
          ],
        ),
      ),
    );
  }
}
