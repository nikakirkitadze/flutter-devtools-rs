import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:jank_lab/main.dart';
import 'package:jank_lab/scenarios/deep_tree.dart';
import 'package:jank_lab/scenarios/home.dart';
import 'package:jank_lab/scenarios/jank.dart';
import 'package:jank_lab/scenarios/leaks.dart';
import 'package:jank_lab/scenarios/logs.dart';
import 'package:jank_lab/scenarios/rebuilds.dart';

void main() {
  testWidgets('home lists six scenarios', (tester) async {
    await tester.pumpWidget(const JankLabApp());
    expect(find.byType(ListTile), findsNWidgets(scenarioRoutes.length));
    expect(scenarioRoutes.length, 6);
  });

  testWidgets('leaky screen keeps its listener after pop', (tester) async {
    await tester.pumpWidget(const JankLabApp());
    final listeners = AppEvents.instance.listenerCountForTesting;
    final jar = LeakJar.controllers.length;

    final navigator = tester.state<NavigatorState>(find.byType(Navigator));
    navigator.pushNamed('/leaks');
    await tester.pumpAndSettle();
    navigator.pop();
    await tester.pumpAndSettle();

    expect(AppEvents.instance.listenerCountForTesting, listeners + 1);
    expect(LeakJar.controllers.length, jar + 1);
  });

  testWidgets('product grid has 64 tiles', (tester) async {
    await tester.pumpWidget(const MaterialApp(home: CartScreen()));
    await tester.pump(const Duration(milliseconds: 50));
    expect(find.byType(ProductTile), findsNWidgets(ProductGrid.tileCount));
    expect(ProductGrid.tileCount, 64);
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('cart ticks rebuild the whole grid', (tester) async {
    await tester.pumpWidget(const MaterialApp(home: CartScreen()));
    final before = ProductTile.buildCount;
    await tester.pump(const Duration(milliseconds: 20));
    expect(ProductTile.buildCount - before, greaterThanOrEqualTo(64));
    final mid = ProductTile.buildCount;
    await tester.pump(const Duration(milliseconds: 20));
    expect(ProductTile.buildCount - mid, greaterThanOrEqualTo(64));
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('deep tree builds the requested shape', (tester) async {
    await tester.pumpWidget(
      const MaterialApp(home: DeepTreeScreen(depth: 20, nodes: 1000)),
    );
    expect(
      find.descendant(of: find.byType(Wrap), matching: find.byType(ColoredBox)),
      findsNWidgets(490),
    );
  });

  testWidgets('heavy build panel builds', (tester) async {
    await tester.pumpWidget(
      const MaterialApp(
        home: Scaffold(
          body: HeavyBuildPanel(buildCost: Duration(milliseconds: 1)),
        ),
      ),
    );
    await tester.pump(const Duration(milliseconds: 100));
    expect(tester.takeException(), isNull);
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('logs screen has its buttons', (tester) async {
    await tester.pumpWidget(const MaterialApp(home: LogsScreen()));
    for (final label in [
      'Emit levels',
      'Print burst',
      'Multi-line record',
      'Report Flutter error',
      'Throw in build',
    ]) {
      expect(find.text(label), findsOneWidget, reason: label);
    }
  });
}
