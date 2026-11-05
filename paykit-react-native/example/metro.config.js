const { getDefaultConfig, mergeConfig } = require('@react-native/metro-config');

// https://reactnative.dev/docs/metro
module.exports = mergeConfig(getDefaultConfig(__dirname), {});
