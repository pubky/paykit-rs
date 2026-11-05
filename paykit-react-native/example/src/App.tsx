// One screen: pick a network mode, run the smoke steps, read pass/fail rows.
import React, {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import {
  ActivityIndicator,
  Platform,
  Pressable,
  SafeAreaView,
  ScrollView,
  StatusBar,
  StyleSheet,
  Text,
  TextInput,
  View,
  useColorScheme,
} from 'react-native';

import {
  defaultHomeserver,
  defaultLocalHost,
  parseLaunchConfig,
  type LaunchConfig,
  type LaunchProps,
  type NetworkMode,
} from './launchConfig';
import { STEPS, runSmoke, type StepResult, type StepStatus } from './smoke';

const LIGHT = {
  background: '#EDF1F0',
  surface: '#FFFFFF',
  ink: '#18201D',
  muted: '#5D6964',
  line: '#D3DBD8',
  accent: '#24476B',
  onAccent: '#FFFFFF',
  pass: '#1E7A4C',
  fail: '#B42318',
  skip: '#8A6D1F',
};
const DARK: typeof LIGHT = {
  background: '#111614',
  surface: '#1B2220',
  ink: '#E4EAE7',
  muted: '#94A19B',
  line: '#2C3532',
  accent: '#8DB4DD',
  onAccent: '#0E1A26',
  pass: '#5CC48D',
  fail: '#F2867A',
  skip: '#D8B65A',
};
type Palette = typeof LIGHT;

const STATUS: Record<
  StepStatus,
  { mark: string; label: string; color: keyof Palette }
> = {
  pending: { mark: '·', label: 'Not run', color: 'muted' },
  running: { mark: '', label: 'Running', color: 'muted' },
  pass: { mark: '✓', label: 'Passed', color: 'pass' },
  fail: { mark: '✕', label: 'Failed', color: 'fail' },
  skip: { mark: '–', label: 'Skipped', color: 'skip' },
};

function initialRows(): StepResult[] {
  return STEPS.map(({ name, title }) => ({
    name,
    title,
    status: 'pending',
    reason: '',
  }));
}

type Counts = { total: number; done: number; failed: number };

function countRows(rows: StepResult[]): Counts {
  const counts = { total: rows.length, done: 0, failed: 0 };
  for (const { status } of rows) {
    if (status === 'pass' || status === 'fail' || status === 'skip') {
      counts.done += 1;
    }
    if (status === 'fail') {
      counts.failed += 1;
    }
  }
  return counts;
}

function verdict({ total, done, failed }: Counts, running: boolean): string {
  if (running) {
    return `Running step ${Math.min(done + 1, total)} of ${total}`;
  }
  if (done === 0) {
    return 'Not run yet';
  }
  if (failed > 0) {
    return failed === 1 ? '1 step failed' : `${failed} steps failed`;
  }
  return 'All steps passed';
}

export default function App(props: LaunchProps): React.JSX.Element {
  const colors = useColorScheme() === 'dark' ? DARK : LIGHT;
  const styles = useMemo(() => makeStyles(colors), [colors]);
  const [config, setConfig] = useState<LaunchConfig>(() =>
    parseLaunchConfig(props, Platform.OS)
  );
  const [rows, setRows] = useState<StepResult[]>(initialRows);
  const [running, setRunning] = useState(false);
  const autorunStarted = useRef(false);

  const run = useCallback(async () => {
    setRunning(true);
    setRows(initialRows());
    try {
      await runSmoke(config, Platform.OS, (result) =>
        setRows((current) =>
          current.map((row) => (row.name === result.name ? result : row))
        )
      );
    } finally {
      setRunning(false);
    }
  }, [config]);

  useEffect(() => {
    if (config.autorun && !autorunStarted.current) {
      autorunStarted.current = true;
      run();
    }
  }, [config.autorun, run]);

  const setMode = (mode: NetworkMode) =>
    setConfig((current) => ({
      ...current,
      mode,
      homeserver: defaultHomeserver(mode),
      localHost: current.localHost || defaultLocalHost(Platform.OS),
    }));
  const counts = countRows(rows);
  const finished = !running && counts.done > 0;

  return (
    <SafeAreaView style={styles.screen}>
      <StatusBar
        barStyle={colors === DARK ? 'light-content' : 'dark-content'}
        backgroundColor={colors.background}
      />
      <ScrollView contentContainerStyle={styles.content}>
        <Text style={styles.title} accessibilityRole="header">
          Paykit smoke test
        </Text>
        <Text
          style={[
            styles.verdict,
            finished && {
              color: counts.failed > 0 ? colors.fail : colors.pass,
            },
          ]}
          accessibilityLiveRegion="polite"
        >
          {verdict(counts, running)}
        </Text>

        <View style={styles.panel}>
          <Text style={styles.label}>Network</Text>
          <View style={styles.segments} accessibilityRole="radiogroup">
            {(['staging', 'local'] as const).map((mode) => {
              const selected = config.mode === mode;
              return (
                <Pressable
                  key={mode}
                  onPress={() => setMode(mode)}
                  disabled={running}
                  accessibilityRole="radio"
                  accessibilityState={{ selected, disabled: running }}
                  style={[styles.segment, selected && styles.segmentSelected]}
                >
                  <Text
                    style={[
                      styles.segmentText,
                      selected && styles.segmentTextSelected,
                    ]}
                  >
                    {mode === 'staging' ? 'Staging' : 'Local stack'}
                  </Text>
                </Pressable>
              );
            })}
          </View>

          {config.mode === 'staging' ? (
            <>
              <Text style={styles.hint}>
                New accounts need a single-use invite code from the Pubky
                staging homeserver. Leave a field empty to sign in to an
                existing account.
              </Text>
              {(['signupTokenA', 'signupTokenB'] as const).map((key) => (
                <TextInput
                  key={key}
                  style={styles.input}
                  value={config[key] ?? ''}
                  onChangeText={(text) =>
                    setConfig((current) => ({
                      ...current,
                      [key]: text.trim() || undefined,
                    }))
                  }
                  editable={!running}
                  secureTextEntry
                  autoCapitalize="none"
                  autoCorrect={false}
                  placeholder={`Invite code for identity ${key.slice(-1)}`}
                  placeholderTextColor={colors.muted}
                  accessibilityLabel={`Invite code for identity ${key.slice(-1)}`}
                />
              ))}
            </>
          ) : (
            <>
              <Text style={styles.hint}>
                Host running the local Pubky stack. Start it with npm run
                local-stack in the example folder.
              </Text>
              <TextInput
                style={styles.input}
                value={config.localHost}
                onChangeText={(localHost) =>
                  setConfig((current) => ({ ...current, localHost }))
                }
                editable={!running}
                autoCapitalize="none"
                autoCorrect={false}
                keyboardType="url"
                accessibilityLabel="Local stack host"
              />
            </>
          )}

          <Pressable
            onPress={run}
            disabled={running}
            accessibilityRole="button"
            accessibilityState={{ disabled: running, busy: running }}
            style={({ pressed }) => [
              styles.button,
              (pressed || running) && styles.buttonDimmed,
            ]}
          >
            <Text style={styles.buttonText}>
              {running ? 'Running smoke test' : 'Run smoke test'}
            </Text>
          </Pressable>
        </View>

        <View style={styles.steps}>
          {rows.map((row) => (
            <View
              key={row.name}
              style={styles.step}
              accessible
              accessibilityLabel={`${row.title}: ${STATUS[row.status].label}${
                row.reason ? `, ${row.reason}` : ''
              }`}
            >
              <View style={styles.mark}>
                {row.status === 'running' ? (
                  <ActivityIndicator size="small" color={colors.accent} />
                ) : (
                  <Text
                    style={[
                      styles.markText,
                      { color: colors[STATUS[row.status].color] },
                    ]}
                  >
                    {STATUS[row.status].mark}
                  </Text>
                )}
              </View>
              <View style={styles.stepBody}>
                <Text style={styles.stepTitle}>{row.title}</Text>
                {row.reason ? (
                  <Text
                    style={[
                      styles.stepReason,
                      row.status === 'fail' && { color: colors.fail },
                    ]}
                  >
                    {row.reason}
                  </Text>
                ) : null}
              </View>
            </View>
          ))}
        </View>
      </ScrollView>
    </SafeAreaView>
  );
}

function makeStyles(colors: Palette) {
  return StyleSheet.create({
    screen: {
      flex: 1,
      backgroundColor: colors.background,
      // Android draws edge to edge when targeting SDK 36, and SafeAreaView
      // only insets on iOS.
      paddingTop: Platform.OS === 'android' ? StatusBar.currentHeight : 0,
    },
    content: { padding: 20, paddingBottom: 48, gap: 20 },
    title: { fontSize: 15, fontWeight: '600', color: colors.muted },
    verdict: {
      fontSize: 34,
      lineHeight: 40,
      fontWeight: '700',
      letterSpacing: -0.5,
      color: colors.ink,
      fontVariant: ['tabular-nums'],
    },
    panel: {
      backgroundColor: colors.surface,
      borderRadius: 14,
      padding: 16,
      gap: 12,
    },
    label: { fontSize: 15, fontWeight: '600', color: colors.ink },
    hint: { fontSize: 14, lineHeight: 20, color: colors.muted },
    segments: {
      flexDirection: 'row',
      borderWidth: 1,
      borderColor: colors.line,
      borderRadius: 10,
      overflow: 'hidden',
    },
    segment: {
      flex: 1,
      minHeight: 44,
      alignItems: 'center',
      justifyContent: 'center',
    },
    segmentSelected: { backgroundColor: colors.accent },
    segmentText: { fontSize: 15, color: colors.ink },
    segmentTextSelected: { color: colors.onAccent, fontWeight: '600' },
    input: {
      minHeight: 44,
      borderWidth: 1,
      borderColor: colors.line,
      borderRadius: 10,
      paddingHorizontal: 12,
      fontSize: 15,
      color: colors.ink,
    },
    button: {
      minHeight: 48,
      borderRadius: 10,
      alignItems: 'center',
      justifyContent: 'center',
      backgroundColor: colors.accent,
    },
    buttonDimmed: { opacity: 0.6 },
    buttonText: { fontSize: 16, fontWeight: '600', color: colors.onAccent },
    steps: { gap: 2 },
    step: {
      flexDirection: 'row',
      gap: 12,
      paddingVertical: 12,
      borderBottomWidth: StyleSheet.hairlineWidth,
      borderBottomColor: colors.line,
    },
    mark: { width: 24, alignItems: 'center', paddingTop: 1 },
    markText: { fontSize: 18, fontWeight: '700' },
    stepBody: { flex: 1, gap: 2 },
    stepTitle: { fontSize: 16, lineHeight: 22, color: colors.ink },
    stepReason: { fontSize: 13, lineHeight: 18, color: colors.muted },
  });
}
