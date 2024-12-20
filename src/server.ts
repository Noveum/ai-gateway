#!/usr/bin/env node

import { config } from 'dotenv';
import { serve } from '@hono/node-server';
import { app } from './index';
import { ElasticsearchExporter, MetricsExportManager } from './metrics/exporters';

// Load environment variables from .dev.vars
config({ path: '.dev.vars' });

// Initialize metrics exporters for Node.js environment
const initializeMetricsExporters = () => {
  const manager = MetricsExportManager.getInstance();

  if (manager.isInitialized()) {
    return;
  }

  if (!process.env.ELASTICSEARCH_HOST) {
    console.warn('[Metrics] ELASTICSEARCH_HOST not found in environment variables');
    return;
  }

  try {
    manager.addExporter(new ElasticsearchExporter({
      host: process.env.ELASTICSEARCH_HOST,
      port: process.env.ELASTICSEARCH_PORT || '9200',
      username: process.env.ELASTICSEARCH_USERNAME || 'elastic',
      password: process.env.ELASTICSEARCH_PASSWORD!,
      index: process.env.ELASTICSEARCH_INDEX || 'metrics'
    }));
    console.info('[Metrics] Successfully initialized Elasticsearch exporter');
  } catch (error) {
    console.error('[Metrics] Failed to initialize Elasticsearch exporter:', error);
  }
};

// Initialize metrics exporters before starting the server
initializeMetricsExporters();

// Extract the port number from the command line arguments
const defaultPort = 3000;
const args = process.argv.slice(2);
const portArg = args.find((arg) => arg.startsWith('--port='));
const port = portArg ? parseInt(portArg.split('=')[1]) : defaultPort;

serve({
    fetch: app.fetch,
    port: port,
});

const url = `http://localhost:${port}`;

// Server startup message
console.log('\x1b[1m%s\x1b[0m', '🚀 AI Gateway running at:');
console.log('   ' + '\x1b[1;4;32m%s\x1b[0m', `${url}`);
console.log('\n\x1b[32m✨ Ready for connections!\x1b[0m');
