/**
 * The main fixture service ("users-service"). Every decorator here exists to exercise a
 * Parity Checklist row; the row is named next to it.
 */
import { Body, Controller, Delete, Get, Head, Module, Param, Patch, Post, Put, Query } from '@nestjs/common';
import {
    ApiBearerAuth,
    ApiBody,
    ApiExtension,
    ApiHeader,
    ApiOperation,
    ApiProperty,
    ApiQuery,
    ApiSecurity,
    ApiTags
} from '@nestjs/swagger';
import { ApiMcp, ApiRateLimit } from '@hodfords/nestjs-api-gateway-client';

/** Marker removed by the generator together with the operation's `operationId`. */
export const DROP_OPERATION_ID = 'x-fixture-drop-operation-id';

export class AddressDto {
    @ApiProperty({ description: 'Street line' })
    street: string;

    @ApiProperty()
    city: string;
}

/** `AddressDto` is referenced twice (MCP-12, Deviation D8). */
export class CreateUserDto {
    @ApiProperty({ description: 'Display name' })
    name: string;

    @ApiProperty({ type: () => AddressDto })
    homeAddress: AddressDto;

    @ApiProperty({ type: () => AddressDto })
    workAddress: AddressDto;

    @ApiProperty({ type: [String], required: false })
    tags?: string[];
}

/** A reference cycle (MCP-12). */
export class TreeNodeDto {
    @ApiProperty()
    label: string;

    @ApiProperty({ type: () => [TreeNodeDto] })
    children: TreeNodeDto[];
}

@ApiTags('users')
@Controller('v1/users')
export class UsersController {
    @Get()
    @ApiOperation({ summary: 'List users', description: 'Returns every user.' })
    @ApiQuery({ name: 'page', required: false, type: Number })
    @ApiQuery({ name: 'sort', required: false, enum: ['asc', 'desc'] })
    @ApiMcp('List users')
    listUsers(@Query('page') page?: number): unknown {
        return page;
    }

    @Post()
    @ApiOperation({ summary: 'Create a user' })
    @ApiBody({ type: CreateUserDto, description: 'The new user' })
    @ApiRateLimit(1, 60, 201)
    createUser(@Body() body: CreateUserDto): unknown {
        return body;
    }

    /** Declared before `me`, so `/v1/users/me` matches this Route first (OAS-21). */
    @Get(':id')
    @ApiOperation({ summary: 'Get a user' })
    @ApiSecurity('auth-user-id')
    @ApiHeader({ name: 'auth-user-id', required: false })
    @ApiHeader({ name: 'Permission-In-Any-Guard', required: false })
    @ApiHeader({ name: 'x-request-id', required: false })
    getUser(@Param('id') id: string): unknown {
        return id;
    }

    @Get('me')
    @ApiOperation({ summary: 'The current user' })
    @ApiBearerAuth()
    getMe(): unknown {
        return null;
    }

    @Put(':id')
    @ApiSecurity('x-api-key')
    replaceUser(@Param('id') id: string, @Body() body: CreateUserDto): unknown {
        return [id, body];
    }

    @Patch(':id')
    @ApiSecurity('auth-user-id')
    @ApiSecurity('x-api-key')
    updateUser(@Param('id') id: string): unknown {
        return id;
    }

    @Delete(':id')
    deleteUser(@Param('id') id: string): unknown {
        return id;
    }

    @Head(':id')
    headUser(@Param('id') id: string): unknown {
        return id;
    }
}

@ApiTags('limits')
@Controller('v1/limits')
export class LimitsController {
    @Get('plain')
    @ApiRateLimit(2, 60)
    plain(): unknown {
        return null;
    }

    @Post('on-status')
    @ApiRateLimit(1, 60, 201)
    onStatus(): unknown {
        return null;
    }

    /** Two limits without a status on one Route (RL-17, Deviation D2). */
    @Get('stacked')
    @ApiRateLimit(3, 60)
    @ApiRateLimit(5, 7200)
    stacked(): unknown {
        return null;
    }

    /** A limit without and a limit with a status on one Route. */
    @Get('stacked-status')
    @ApiRateLimit(2, 60)
    @ApiRateLimit(1, 3600, 404)
    stackedStatus(): unknown {
        return null;
    }
}

@ApiTags('paths')
@Controller('v1/paths')
export class PathsController {
    /** `x-router-path` with a `*name` wildcard (OAS-20). */
    @Get('files/:path')
    @ApiExtension('x-router-path', '/v1/paths/files/*path')
    files(@Param('path') path: string): unknown {
        return path;
    }

    /** `x-router-path` with an optional group (OAS-20). */
    @Get('optional')
    @ApiExtension('x-router-path', '/v1/paths/optional{/:id}')
    optional(): unknown {
        return null;
    }

    /** `x-router-path` with a backslash escape (OAS-20). */
    @Get('escaped')
    @ApiExtension('x-router-path', '/v1/paths/escaped\\:literal')
    escaped(): unknown {
        return null;
    }

    /** An operation without `operationId` (OAS-08, MCP-07). */
    @Get('anonymous/:slug')
    @ApiExtension(DROP_OPERATION_ID, true)
    anonymous(@Param('slug') slug: string): unknown {
        return slug;
    }

    /** A JSON body with a reference cycle (MCP-12). */
    @Post('trees')
    @ApiBody({ type: TreeNodeDto, required: true })
    createTree(@Body() body: TreeNodeDto): unknown {
        return body;
    }
}

@Module({ controllers: [UsersController, LimitsController, PathsController] })
export class UsersModule {}

/** The same service with one extra Route, used to swap documents at runtime (OAS-02, OAS-03). */
@ApiTags('v2')
@Controller('v2/new')
export class NewController {
    @Get()
    newRoute(): unknown {
        return null;
    }
}

@Module({ controllers: [UsersController, LimitsController, PathsController, NewController] })
export class UsersV2Module {}
