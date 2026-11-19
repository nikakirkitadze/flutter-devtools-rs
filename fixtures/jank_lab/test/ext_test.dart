import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:jank_lab/ext.dart';
import 'package:jank_lab/main.dart';

void main() {
  const builtins = ['goto_logs', 'goto_network', 'pop', 'list'];

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
}
