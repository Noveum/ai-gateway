import typescript from '@rollup/plugin-typescript';
import terser from '@rollup/plugin-terser';
import json from '@rollup/plugin-json';
import copy from 'rollup-plugin-copy';

export default {
  input: 'src/server.ts',
  output: {
    dir: 'build',
    format: 'es',
  },
  plugins: [
    typescript({ exclude: ['**/*.test.ts', 'cookbook'] }),
    terser(),
    json(),
    copy({
      targets: [{ src: 'src/public/*', dest: 'build/public' }],
    }),
  ],
};
