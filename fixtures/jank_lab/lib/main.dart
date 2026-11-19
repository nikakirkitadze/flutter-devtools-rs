import 'package:flutter/material.dart';

import 'ext.dart';
import 'scenarios/deep_tree.dart';
import 'scenarios/home.dart';
import 'scenarios/jank.dart';
import 'scenarios/leaks.dart';
import 'scenarios/logs.dart';
import 'scenarios/network.dart';
import 'scenarios/rebuilds.dart';

void main() {
  registerJankLabExtension();
  const key = String.fromEnvironment('SCENARIO');
  runApp(JankLabApp(initialRoute: scenarioRoutes[key] ?? '/'));
}

/// The fixture app. [initialRoute] picks the starting scenario.
class JankLabApp extends StatelessWidget {
  const JankLabApp({super.key, this.initialRoute = '/'});

  final String initialRoute;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      navigatorKey: navKey,
      title: 'jank_lab',
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      initialRoute: initialRoute,
      onGenerateRoute: (settings) {
        final Widget page = switch (settings.name) {
          '/jank' => const JankScreen(),
          '/rebuilds' => const CartScreen(),
          '/leaks' => const LeakyScreen(),
          '/network' => const NetworkScreen(),
          '/deep-tree' => const DeepTreeScreen(),
          '/logs' => const LogsScreen(),
          _ => const HomeScreen(),
        };
        return MaterialPageRoute<void>(
          builder: (_) => page,
          settings: settings,
        );
      },
    );
  }
}
