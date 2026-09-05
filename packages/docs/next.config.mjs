import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // ponytail: no landing page to maintain; the guide is the site
  redirects: async () => [{ source: '/', destination: '/docs', permanent: false }],
};

export default withMDX(config);
