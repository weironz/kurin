/**
 * 测试用 MSW server：`setupServer` 在 node 环境拦截 fetch。
 */
import { setupServer } from 'msw/node';
import { handlers } from './handlers';
import { operationDetailHandlers } from './operationHandlers';

export const server = setupServer(...operationDetailHandlers, ...handlers);
