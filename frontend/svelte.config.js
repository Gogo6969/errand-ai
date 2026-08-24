import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

export default {
  preprocess: vitePreprocess(),
  kit: {
    adapter: adapter({ fallback: 'index.html' }),
    // The app is one window talking to a daemon, so everything renders in the
    // client. There is no server to render on.
    prerender: { entries: [] }
  }
};
