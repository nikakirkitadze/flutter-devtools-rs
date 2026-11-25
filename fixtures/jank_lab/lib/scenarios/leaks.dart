import 'package:flutter/material.dart';

/// Provokes retained objects after a screen is closed. `LeakyScreen` leaves a
/// listener on [AppEvents], never disposes a `TextEditingController`, and
/// parks a disposed `ScrollController` in [LeakJar]. Open the screen, go back,
/// then look for the instances in the fdt Memory tool's snapshot diff.
class AppEvents extends ChangeNotifier {
  AppEvents._();

  static final AppEvents instance = AppEvents._();

  int _listeners = 0;

  int get listenerCountForTesting => _listeners;

  @override
  void addListener(VoidCallback listener) {
    _listeners++;
    super.addListener(listener);
  }

  @override
  void removeListener(VoidCallback listener) {
    _listeners--;
    super.removeListener(listener);
  }

  void ping() => notifyListeners();
}

/// Holds controllers that were disposed but are never released.
class LeakJar {
  static final List<ScrollController> controllers = [];
}

/// How many `LeakyScreen` states are mounted right now (initState to dispose).
int leakyScreensMounted = 0;

class LeakyScreen extends StatefulWidget {
  const LeakyScreen({super.key});

  @override
  State<LeakyScreen> createState() => _LeakyScreenState();
}

class _LeakyScreenState extends State<LeakyScreen> {
  // Never disposed.
  final TextEditingController _text = TextEditingController();
  final ScrollController _scroll = ScrollController();
  int _pings = 0;

  @override
  void initState() {
    super.initState();
    // Captures `this` and is never removed.
    AppEvents.instance.addListener(() {
      if (mounted) setState(() => _pings++);
    });
    LeakJar.controllers.add(_scroll);
    leakyScreensMounted++;
  }

  @override
  void dispose() {
    leakyScreensMounted--;
    _scroll.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Leaks')),
      body: ListView(
        controller: _scroll,
        padding: const EdgeInsets.all(16),
        children: [
          TextField(
            controller: _text,
            decoration: const InputDecoration(labelText: 'Type here'),
          ),
          const SizedBox(height: 16),
          Text('Pings seen: $_pings'),
          FilledButton(
            onPressed: AppEvents.instance.ping,
            child: const Text('Ping'),
          ),
        ],
      ),
    );
  }
}
