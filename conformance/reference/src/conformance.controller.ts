import { Controller, Get } from '@nestjs/common';
import { OpenApiService } from '@hodfords/nestjs-api-gateway';

/**
 * Application route used by the scenario runner (CFG-18 lets applications add routes).
 * `rust-gateway-server` serves the same route with the same body.
 */
@Controller('__conformance')
export class ConformanceController {
    constructor(private readonly openApiService: OpenApiService) {}

    /** Answers once every service's first document load has settled (CFG-16). */
    @Get('ready')
    async ready(): Promise<{ services: string[] }> {
        await this.openApiService.ready;
        return { services: Object.keys(this.openApiService.apiDocs) };
    }
}
