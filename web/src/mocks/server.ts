/**
 * 测试用 MSW server：`setupServer` 在 node 环境拦截 fetch。
 */
import { setupServer } from 'msw/node';
import { handlers } from './handlers';

export const server = setupServer(...handlers);
