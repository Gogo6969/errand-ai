import { sveltekit } from '@sveltejs/kit/vite';

export default {
  plugins: [sveltekit()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, watch: { ignored: ['**/target/**'] } }
};
