/** @type {import('next').NextConfig} */
const nextConfig = {
  webpack(config) {
    // Allow Node-ESM-style .js imports to resolve .ts / .tsx source files.
    // The lib/ engine uses 'import foo from "./bar.js"' which is standard ESM
    // but webpack needs an explicit resolver hint.
    config.resolve.extensionAlias = {
      '.js': ['.ts', '.tsx', '.js'],
      '.mjs': ['.mts', '.mjs'],
    }
    return config
  },
}

export default nextConfig
