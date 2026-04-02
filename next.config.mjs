/** @type {import('next').NextConfig} */
const nextConfig = {
  // Static export so Tauri can bundle the built files directly.
  output: "export",
  // Write the export to /dist so tauri.conf.json frontendDist: "dist" resolves correctly.
  distDir: "dist",
  // Disable image optimisation (not available in static export mode).
  images: { unoptimized: true },
};

export default nextConfig;
