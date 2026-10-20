import js from '@eslint/js';
import prettierRecommended from 'eslint-plugin-prettier/recommended';
import { defineConfig } from 'eslint/config';
import globals from 'globals';
import tseslint from 'typescript-eslint';

export default defineConfig([
  {
    // ubrn output (bindings and TurboModule glue) is generated, not linted.
    ignores: [
      'node_modules/',
      'lib/',
      'src/generated/',
      'cpp/',
      'src/index.tsx',
      'src/NativeReactNativePaykit.ts',
      // The example app's dependencies and native projects.
      'example/node_modules/',
      'example/android/',
      'example/ios/',
    ],
  },
  js.configs.recommended,
  tseslint.configs.recommended,
  prettierRecommended,
  {
    files: ['*.js', 'scripts/**', 'example/*.js', 'example/scripts/**'],
    languageOptions: { globals: globals.node },
  },
  {
    // React Native loads the example's Metro and Babel configs as CommonJS.
    files: ['example/*.js'],
    rules: { '@typescript-eslint/no-require-imports': 'off' },
  },
  {
    files: ['src/__tests__/**', 'example/src/__tests__/**'],
    languageOptions: { globals: { ...globals.node, ...globals.jest } },
    rules: {
      // The tests load modules with require() after jest.mock().
      '@typescript-eslint/no-require-imports': 'off',
    },
  },
]);
