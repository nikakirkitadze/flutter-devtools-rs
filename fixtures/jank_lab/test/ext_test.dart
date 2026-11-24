import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:jank_lab/ext.dart';
import 'package:jank_lab/main.dart';
import 'package:jank_lab/scenarios/leaks.dart';
import 'package:jank_lab/scenarios/rebuilds.dart';

void main() {
  const builtins = [
    'goto_logs',
    'goto_network',
    'goto_jank',
    'goto_rebuilds',
    'goto_leaks',
    'pump_frames',
    'home',
    'pop',
    'list',
  ];

  testWidgets('list returns the built-in actions', (tester) async {
    await tester.pumpWidget(const JankLabApp());
    final response = await handleJankLabRun('list');
    expect(response.isError(), isFalse);
    expect(jankLabAvailable(), containsAll(builtins));
  });

  testWidgets('an unknown action is an invalid-params error', (tester) async {
    await tester.pumpWidget(const JankLabApp());
    final response = await handleJankLabRun('nope');
    expect(response.isError(), isTrue);
    expect(response.errorCode, -32602);
    expect(response.errorDetail, 'Unknown action: nope');
  });

  testWidgets('the Logs screen registers its actions while mounted', (
    tester,
  ) async {
    await tester.pumpWidget(const JankLabApp());
    expect(jankLabAvailable(), isNot(contains('emit_levels')));

    await handleJankLabRun('goto_logs');
    await tester.pumpAndSettle();
    expect(
      jankLabAvailable(),
      containsAll([
        'emit_levels',
        'print_burst',
        'multi_line',
        'report_error',
        'throw_in_build',
      ]),
    );
    expect((await handleJankLabRun('emit_levels')).isError(), isFalse);

    await handleJankLabRun('pop');
    await tester.pumpAndSettle();
    expect(jankLabAvailable(), isNot(contains('emit_levels')));
    expect((await handleJankLabRun('emit_levels')).isError(), isTrue);
  });

  testWidgets('the Network screen registers its actions while mounted', (
    tester,
  ) async {
    await tester.pumpWidget(const JankLabApp(initialRoute: '/network'));
    await tester.pump();
    expect(
      jankLabAvailable(),
      containsAll([
        'load_products',
        'place_order',
        'download',
        'burst',
        'open_socket',
        'hero_image',
        'close_socket',
        'refused',
        'slow',
      ]),
    );
    await tester.pumpWidget(const SizedBox());
    expect(jankLabAvailable(), isNot(contains('load_products')));
  });

  testWidgets('goto_jank, raster_on and raster_off drive the Jank screen', (
    tester,
  ) async {
    await tester.pumpWidget(const JankLabApp());
    expect(jankLabAvailable(), isNot(contains('raster_on')));

    expect((await handleJankLabRun('goto_jank')).isError(), isFalse);
    await tester.pump();
    await tester.pump(const Duration(seconds: 1));
    expect(find.text('Jank'), findsOneWidget);
    expect(jankLabAvailable(), containsAll(['raster_on', 'raster_off']));
    bool switchValue() =>
        tester.widget<SwitchListTile>(find.byType(SwitchListTile)).value;
    expect(switchValue(), isFalse);

    await handleJankLabRun('raster_on');
    await tester.pump();
    expect(switchValue(), isTrue);
    await handleJankLabRun('raster_off');
    await tester.pump();
    expect(switchValue(), isFalse);

    await handleJankLabRun('home');
    await tester.pump();
    await tester.pump(const Duration(seconds: 1));
    expect(find.text('Jank'), findsNothing);
    expect(jankLabAvailable(), isNot(contains('raster_on')));
  });

  testWidgets('goto_rebuilds pushes the cart screen', (tester) async {
    await tester.pumpWidget(const JankLabApp());
    await handleJankLabRun('goto_rebuilds');
    await tester.pump();
    await tester.pump(const Duration(seconds: 1));
    expect(find.byType(ProductTile), findsWidgets);
    await handleJankLabRun('home');
    await tester.pump();
    await tester.pump(const Duration(seconds: 1));
    expect(find.byType(ProductTile), findsNothing);
  });

  testWidgets('goto_leaks pushes the leaky screen and home pops it', (
    tester,
  ) async {
    await tester.pumpWidget(const JankLabApp());
    expect(find.byType(LeakyScreen), findsNothing);
    expect((await handleJankLabRun('goto_leaks')).isError(), isFalse);
    await tester.pump();
    await tester.pump(const Duration(seconds: 1));
    expect(find.byType(LeakyScreen), findsOneWidget);
    await handleJankLabRun('home');
    await tester.pump();
    await tester.pump(const Duration(seconds: 1));
    expect(find.byType(LeakyScreen), findsNothing);
  });

  testWidgets('pump_frames returns ok at once and is listed', (tester) async {
    await tester.pumpWidget(const JankLabApp());
    final response = await handleJankLabRun('pump_frames');
    expect(response.isError(), isFalse);
    expect(jankLabAvailable(), contains('pump_frames'));
    // Let its 30 timer ticks run out, so no timer is left pending.
    await tester.pump(const Duration(seconds: 1));
  });
}
