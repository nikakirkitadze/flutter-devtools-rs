import 'package:flutter/material.dart';

/// Scenario keys (as passed in `--dart-define=SCENARIO=<key>`) and their routes.
const Map<String, String> scenarioRoutes = {
  'jank': '/jank',
  'rebuilds': '/rebuilds',
  'leaks': '/leaks',
  'network': '/network',
  'deep-tree': '/deep-tree',
  'logs': '/logs',
};

/// The list of scenarios. Each tile opens one deliberate problem.
class HomeScreen extends StatelessWidget {
  const HomeScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('jank_lab')),
      body: ListView(
        children: [
          for (final entry in scenarioRoutes.entries)
            ListTile(
              title: Text(entry.key),
              subtitle: Text(entry.value),
              trailing: const Icon(Icons.chevron_right),
              onTap: () => Navigator.of(context).pushNamed(entry.value),
            ),
        ],
      ),
    );
  }
}
