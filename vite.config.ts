import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**", "**/crates/**", "**/target/**"] },
  },
  build: {
    target: "chrome105",
    minify: "esbuild",
    rollupOptions: {
      input: {
        main: "index.html",
        preview: "preview.html",
      },
    },
  },
});
