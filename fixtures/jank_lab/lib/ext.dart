import 'dart:async';
import 'dart:convert';
import 'dart:developer' as developer;

import 'package:flutter/widgets.dart';

/// Navigator of the fixture app, so automation can push routes without a
/// [BuildContext].
final GlobalKey<NavigatorState> navKey = GlobalKey<NavigatorState>();

/// Actions registered by whichever scenario screen is currently mounted.
final Map<String, FutureOr<void> Function()> jankActions = {};

/// Actions that work on every screen.
const List<String> jankBuiltins = ['goto_logs', 'goto_network', 'pop', 'list'];

/// Every action that can be run right now, built-ins first.
List<String> jankLabAvailable() => [...jankBuiltins, ...jankActions.keys];

/// Registers [actions] and returns a function that removes exactly those
/// entries. Call it from `initState` and call the result from `dispose`.
VoidCallback registerJankActions(
  Map<String, FutureOr<void> Function()> actions,
) {
  jankActions.addAll(actions);
  return () {
    for (final entry in actions.entries) {
      if (identical(jankActions[entry.key], entry.value)) {
        jankActions.remove(entry.key);
      }
    }
  };
}

/// Registers `ext.jank_lab.run`. Call once, before `runApp`.
void registerJankLabExtension() {
  developer.registerExtension(
    'ext.jank_lab.run',
    (method, params) => handleJankLabRun(params['action'] ?? ''),
  );
}

/// Runs [action] and answers as the service extension does. The action is not
/// awaited, so a long one returns at once. Exported for tests.
Future<developer.ServiceExtensionResponse> handleJankLabRun(
  String action,
) async {
  switch (action) {
    case 'goto_logs':
      navKey.currentState?.pushNamed('/logs');
    case 'goto_network':
      navKey.currentState?.pushNamed('/network');
    case 'pop':
      navKey.currentState?.pop();
    case 'list':
      break;
    default:
      final fn = jankActions[action];
      if (fn == null) {
        return developer.ServiceExtensionResponse.error(
          developer.ServiceExtensionResponse.invalidParams,
          'Unknown action: $action',
        );
      }
      unawaited(Future.sync(fn));
  }
  // A window that is not in front draws no frames, so a pushed route would
  // never build.
  WidgetsBinding.instance.scheduleFrame();
  return developer.ServiceExtensionResponse.result(
    jsonEncode({'ok': true, 'available': jankLabAvailable()}),
  );
}
