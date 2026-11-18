/**
 * The second fixture service ("default-service"), used as the Default Service and as the
 * owner of Direct Prefixes (REQ-04, REQ-07, FWD-02).
 */
import { Controller, Get, Module, Param, Post } from '@nestjs/common';
import { ApiTags } from '@nestjs/swagger';
import { ApiRateLimit } from '@hodfords/nestjs-api-gateway-client';

@ApiTags('oauth')
@Controller('oauth')
export class OauthController {
    @Post('token')
    @ApiRateLimit(2, 60)
    token(): unknown {
        return null;
    }

    @Get('authorize')
    authorize(): unknown {
        return null;
    }
}

@ApiTags('default')
@Controller()
export class DefaultController {
    @Get('health')
    health(): unknown {
        return null;
    }

    @Get('v1/items/:id')
    item(@Param('id') id: string): unknown {
        return id;
    }

    /** Same method and pattern as `users-service` `GET /v1/limits/plain` (RL-17, Deviation D2). */
    @Get('v1/limits/plain')
    @ApiRateLimit(2, 60)
    plain(): unknown {
        return null;
    }
}

@Module({ controllers: [OauthController, DefaultController] })
export class DefaultModule {}
